// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-bin15 — rolling-binary member (slot 3)
//!
//! The coded carrier of the BIN15 lane: Hyperliquid HIP-4 outcome
//! markets (`docs/research/outcome/03-implementation-spec-2026-09-12.md`
//! §6.4, and the plan and capture analyses beside it — all in the
//! git-excluded research vault). A FAMILY is a stable pair of
//! `SymbolId` slots — Yes and No — whose underlying venue instrument is
//! rebound every time an instance settles and the next one is created;
//! the BTC 15-minute family rolls 96 times a day. The member prices
//! each live instance off a `core-vol` 15 m volatility forecast of the
//! family's UNDERLYING, compares that fair value with the binary's own
//! book, and acts when the book is wrong by more than the configured
//! edge.
//!
//! The pricer, per re-price, is: the log-moneyness of the underlying
//! mark against the strike to second order; divided by `σ_min·√τ` built
//! from the once-a-minute `sigma_hat_1e9`; looked up in a Φ table on a
//! 1e-3 grid; then recalibrated through one of three τ-phase tables.
//! One `i128` mul-div, one integer square root, four table reads. No
//! float anywhere, no transcendental on the decision path — the two
//! expensive functions (`exp`, `ln`) run once per minute per underlying
//! inside `core-vol`, which is the crate's standing doctrine.
//!
//! ## What this lane does NOT do
//!
//! * **It does not infer a position from a submit.** Positions come
//!   from fills. Before the engine-side paper matcher (VRP P1) exists,
//!   live paper produces no fills and this member is a PIPELINE PROOF:
//!   it prices, it emits, it counts. The harness (`backtest --member
//!   bin15`) is the P&L instrument. A member that booked its submits
//!   is how the VRP member came to believe it held a hedged option for
//!   two live campaigns while the harness held a naked perp.
//! * **It never sells short.** A HIP-4 position cannot go negative: the
//!   two sides of a binary are two separate instruments, and the way to
//!   express "Yes is rich" is to BUY No, not to sell Yes. The only sell
//!   this member ever emits is a CLOSING take against inventory it can
//!   prove it holds from fills.
//! * **It never takes in the last `tail_refuse_ns`.** At the very end
//!   of a binary's life the fair value is a step function of the next
//!   tick of the underlying, and the model's own error is larger than
//!   any edge it can measure. The venue also clears the book at expiry.
//! * **It never quotes inside `tau_min_quote_ns`.** A resting quote
//!   near expiry is pure gamma to whoever crosses it.
//! * **It does not consult the regime word.** The HORIZON law: the
//!   regime lane is measured on 4 h–8 h horizons and a 15-minute binary
//!   is not one of its cells. The gate is accepted and ignored, and
//!   that is deliberate rather than an omission.
//! * **It does not build a signer, a cancel or a dispatch path.** Those
//!   are Stage-3.
//!
//! ## Arm B is a LOWER BOUND, and why
//!
//! An ask on the Yes book with no Yes inventory is a short, which the
//! venue refuses and the paper model does not know. So Arm B quotes the
//! ask side only up to `pos_yes`; the bid side is the primary maker
//! quote and the No book mirrors it against `pos_no`. A real two-sided
//! maker on a venue that allowed shorting would earn more than this
//! member's paper P&L shows. The bound is in the conservative
//! direction, which is the only direction a paper model may be wrong
//! in.
//!
//! ## Doctrine
//!
//! No allocation after `new` (the LUT box and the vol engines are the
//! only heap, all taken at boot). `#[repr(C)]` PODs, hot structs
//! cache-line aligned. No `dyn`. No floats. `debug_assert!` for the
//! invariants, fail-fast in release. While-index loops, no iterators on
//! the decision path.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core_fill::{Touch, ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_time::{BarClock, NsTs};
use core_types::{
    AiCmdKind, Order, Price, Qty, Side, SymbolId, Tick, VenueId, SYMBOL_ID_NONE,
};
use strategy_core::{
    Bin15Counters, Ctx, RegimeGate, Strategy, StrategyCounters, StrategyError, SubmitErr,
};

pub mod price;

/// Families one member instance can carry. Mirrors
/// `core_config::universe::HL_ROLLING_MAX` and
/// `ingress_hyperliquid::family::HL_MAX_FAMILIES`; the three are one
/// number and a boot that disagrees is refused by `bin15_boot`.
pub const BIN15_MAX_FAMILIES: usize = 8;

/// Distinct family underlyings one member instance can carry (BTC, ETH,
/// SOL, HYPE). Two families may share one underlying — a 15 m and a
/// daily on BTC — and then share its `MarkState`.
pub const BIN15_MAX_UNDERLYINGS: usize = 4;

/// Arm B quote sides per family: index 0 = bid, 1 = ask.
pub const QUOTE_SIDES: usize = 2;

/// The 15 m `core-vol` tenor, ns. The 15 m families' σ source.
pub const TAU_15M_NS: u64 = 900_000_000_000;

/// The 8 h `core-vol` tenor, ns. The NATIVE DAILY families' σ source:
/// a 24 h binary looked at 1–12 h before expiry needs a horizon the
/// HAR was measured on, scaled by τ — not a 24 h HAR the crate refuses
/// to forecast (spec §6.4.6, ruling O-Q7).
pub const TAU_8H_NS: u64 = 28_800_000_000_000;

/// Minutes in the 15 m tenor — the divisor that turns `σ_τ` into a
/// per-minute variance.
const TAU_15M_MINUTES: i128 = 15;

/// Minutes in the 8 h tenor.
const TAU_8H_MINUTES: i128 = 480;

/// HIP-4 price tick ×1e6: 1e-4 of a dollar of payout.
pub const GRID_TICK_1E6: i64 = 100;

/// HIP-4 lot ×1e6: one whole contract.
pub const GRID_LOT_1E6: i64 = 1_000_000;

/// Lowest quotable binary price ×1e6 (0.001).
pub const GRID_PX_MIN_1E6: i64 = 1_000;

/// Highest quotable binary price ×1e6 (0.999).
pub const GRID_PX_MAX_1E6: i64 = 999_000;

/// Minimum order notional ×1e6 ($10).
pub const GRID_MIN_NOTIONAL_1E6: i128 = 10_000_000;

/// One whole payout ×1e6 — a binary settles at 0 or at this.
pub const ONE_1E6: i64 = 1_000_000;

/// `arm` value of the model arm.
pub const ARM_MODEL: u8 = 0;
/// `arm` value of the null arm (the venue mid in place of `p̂`).
pub const ARM_NULL: u8 = 1;

/// Family kind: a 15-minute rolling binary (`out:<COIN>:15m`).
pub const FAMILY_OUT_15M: u8 = 0;
/// Family kind: a native recurring daily binary (`native:<COIN>:1d`).
pub const FAMILY_NATIVE_DAILY: u8 = 1;

/// `client_oid` bit 63: the arm (0 model, 1 null).
const OID_ARM_SHIFT: u32 = 63;
/// `client_oid` bit 48: the side (0 Yes, 1 No).
const OID_SIDE_SHIFT: u32 = 48;
/// `client_oid` bits 32..48: the family index.
const OID_FAMILY_SHIFT: u32 = 32;
/// `client_oid` bits 0..32: the outcome id.
const OID_OUTCOME_MASK: u64 = 0xFFFF_FFFF;

/// Nanoseconds in a day — the day-cap epoch.
const DAY_NS: u64 = 86_400_000_000_000;

/// One HIP-4 instance as the member knows it. POD; `outcome == 0` means
/// the slot is DORMANT — no instance is live and nothing may be emitted.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BinarySpec {
    /// Venue outcome id. `0` = none.
    pub outcome: u32,
    /// Explicit padding — always zero.
    pub _pad: u32,
    /// Threshold price of the UNDERLYING ×1e6.
    pub strike_1e6: i64,
    /// Settlement instant, epoch ns.
    pub expiry_ns: u64,
    /// Settlement TWAP window, ns. `0` = settle at `expiry_ns`.
    pub twap_ns: u64,
    /// When this instance was bound, epoch ns.
    pub created_ns: u64,
}

/// A touch plus the freshness the fill law needs. `core_fill::Touch`
/// carries the four prices; this adds WHEN and whether the last tick
/// was stale, which is the difference between "the book is thin" and
/// "we have no book".
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TouchState {
    /// The four prices ×1e6.
    pub touch: Touch,
    /// When the touch was last written, engine ns.
    pub ts_ns: u64,
    /// `1` once a stale tick arrived; cleared by the next fresh one.
    pub stale: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 7],
}

impl TouchState {
    /// Whether this touch may be acted on: fresh and two-sided.
    #[inline(always)]
    #[must_use]
    pub const fn actionable(&self) -> bool {
        self.stale == 0 && self.touch.bid_1e6 > 0 && self.touch.ask_1e6 > self.touch.bid_1e6
    }
}

/// One in-flight order of this member. `oid == 0` means the slot is
/// free — a `client_oid` of 0 is never emitted, because bits 0..32 of
/// every oid carry a non-zero outcome id.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingLeg {
    /// The `client_oid` submitted.
    pub oid: u64,
    /// Deadline, engine ns: at or after this the leg is written off.
    pub deadline_ns: u64,
    /// Price submitted ×1e6.
    pub px_1e6: i64,
    /// Quantity submitted ×1e6.
    pub qty_1e6: i64,
    /// Quantity filled so far ×1e6.
    pub filled_1e6: i64,
    /// `1` Yes leg, `0` No leg.
    pub is_yes: u8,
    /// [`Side`] as a byte.
    pub side: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 6],
}

impl PendingLeg {
    /// Whether this slot holds a live order.
    #[inline(always)]
    #[must_use]
    pub const fn live(&self) -> bool {
        self.oid != 0
    }
}

/// One configured family and everything the member knows about it.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug)]
pub struct FamilyState {
    /// The live instance; `outcome == 0` = dormant.
    pub live: BinarySpec,
    /// The Yes slot.
    pub sym_yes: SymbolId,
    /// The No slot (the next ordinal).
    pub sym_no: SymbolId,
    /// Index into [`Bin15Strategy`]'s marks.
    pub underlying: u8,
    /// [`FAMILY_OUT_15M`] or [`FAMILY_NATIVE_DAILY`].
    pub kind: u8,
    /// [`ARM_MODEL`] or [`ARM_NULL`] for this instance.
    pub arm: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 1],
    /// The Yes book.
    pub touch_yes: TouchState,
    /// The No book.
    pub touch_no: TouchState,
    /// Last fair value ×1e6.
    pub p_hat_1e6: i64,
    /// Last raw (pre-recalibration) fair value ×1e6 — the ledger's
    /// second column, and the only way to tell a bad forecast from a
    /// bad recalibration.
    pub p_raw_1e6: i64,
    /// When the fair value was last written, engine ns.
    pub p_ts_ns: u64,
    /// Yes contracts held ×1e6, FROM FILLS.
    pub pos_yes_1e6: i64,
    /// No contracts held ×1e6, FROM FILLS.
    pub pos_no_1e6: i64,
    /// The one in-flight take.
    pub pend_take: PendingLeg,
    /// The in-flight Arm B quotes, indexed by [`QUOTE_SIDES`].
    pub pend_quote: [PendingLeg; QUOTE_SIDES],
    /// Notional booked against `cap_instance_usd_1e6` ×1e6.
    pub notional_instance_1e6: i64,
    /// The `p̂` the resting quotes were built from ×1e6, per side.
    pub last_quote_p_1e6: [i64; QUOTE_SIDES],
    /// BIN15 O6: the standardised distance the last reprice priced at
    /// ×1e6 (the pricer's `d`, already clamped).
    pub d_1e6: i64,
    /// BIN15 O6: σ√τ ×1e9 behind that `d` — a small denominator is
    /// what an overconfident forecast looks like.
    pub den_1e9: i64,
    /// BIN15 O6: the mark the last reprice used ×1e6.
    pub mark_1e6: i64,
    /// BIN15 O6: the pricing horizon the last reprice used, ns.
    pub last_tau_ns: u64,
    /// BIN15 O9: `1` once this instance's COVERAGE ENTRY has been
    /// emitted. Its own flag rather than a test on
    /// `notional_instance_1e6`, because the MAKER arm books notional
    /// too — a resting quote would otherwise read as "already
    /// entered" and the coverage entry would never fire.
    pub covered: u8,
}

impl Default for FamilyState {
    fn default() -> Self {
        Self {
            live: BinarySpec::default(),
            sym_yes: SYMBOL_ID_NONE,
            sym_no: SYMBOL_ID_NONE,
            underlying: 0,
            kind: FAMILY_OUT_15M,
            arm: ARM_MODEL,
            _pad: [0; 1],
            touch_yes: TouchState::default(),
            touch_no: TouchState::default(),
            p_hat_1e6: 0,
            p_raw_1e6: 0,
            p_ts_ns: 0,
            pos_yes_1e6: 0,
            pos_no_1e6: 0,
            pend_take: PendingLeg::default(),
            pend_quote: [PendingLeg::default(); QUOTE_SIDES],
            notional_instance_1e6: 0,
            last_quote_p_1e6: [0; QUOTE_SIDES],
            d_1e6: 0,
            den_1e9: 0,
            mark_1e6: 0,
            last_tau_ns: 0,
            covered: 0,
        }
    }
}

impl FamilyState {
    /// Whether an instance is live on this slot.
    #[inline(always)]
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.live.outcome != 0
    }

    /// Drop everything that belonged to the outgoing instance. The
    /// SLOT survives (that is the point of a rolling family); the
    /// instance does not, and neither does anything derived from it —
    /// a position, a touch or a fair value carried across a roll would
    /// be attributed to an instrument that no longer exists.
    fn clear_instance(&mut self) {
        self.live = BinarySpec::default();
        self.touch_yes = TouchState::default();
        self.touch_no = TouchState::default();
        self.p_hat_1e6 = 0;
        self.p_raw_1e6 = 0;
        self.p_ts_ns = 0;
        self.pos_yes_1e6 = 0;
        self.pos_no_1e6 = 0;
        self.pend_take = PendingLeg::default();
        self.pend_quote = [PendingLeg::default(); QUOTE_SIDES];
        self.notional_instance_1e6 = 0;
        self.last_quote_p_1e6 = [0; QUOTE_SIDES];
        // BIN15 O6: the diagnostics describe the instance that is
        // going away, so they go with it — a stale `d` read against
        // the successor's strike is worse than no reading at all.
        self.d_1e6 = 0;
        self.den_1e9 = 0;
        self.mark_1e6 = 0;
        self.last_tau_ns = 0;
        // BIN15 O9: the successor is its own instance and gets its own
        // coverage entry.
        self.covered = 0;
    }
}

/// One underlying's mark, its minute grid and its forecast engines.
///
/// TWO engines per underlying, indexed by family kind. They share the
/// same 1-minute close series — the underlying has one price — but they
/// arm and settle on different horizons, and `core-vol` arms ONE hold
/// per engine. A 15-minute family and a daily family on the same coin
/// would otherwise fight over the same pending hold, and the loser
/// would score its pair against the winner's window. This is ruling
/// O-Q7's cost, paid explicitly.
#[repr(C, align(64))]
pub struct MarkState {
    /// Last mark ×1e6.
    pub mark_1e6: i64,
    /// When the mark was written, engine ns.
    pub ts_ns: u64,
    /// The minute currently open (`BarClock::bar_id`); `0` = none yet.
    pub minute_id: u64,
    /// Last per-minute variance of log-price ×1e18, per family kind.
    pub sig2_min_1e18: [i128; 2],
    /// The forecast engines, indexed by family kind.
    pub vol: [Box<core_vol::VolEngine>; 2],
}

impl MarkState {
    /// A cold underlying. Allocates the two engines — boot only.
    #[must_use]
    fn new() -> Self {
        Self {
            mark_1e6: 0,
            ts_ns: 0,
            minute_id: 0,
            sig2_min_1e18: [0; 2],
            vol: [
                Box::new(core_vol::VolEngine::new()),
                Box::new(core_vol::VolEngine::new()),
            ],
        }
    }
}

/// The member's configuration, as the cli hands it in.
///
/// `core-config` is deliberately NOT a dependency: the cli parses
/// `bin15.toml`, validates every bound, and passes this struct. A
/// member that parsed its own TOML would be a second grammar to keep
/// in step with the artifact hash the operator reads at boot.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bin15Params {
    /// Yes slot per family; [`SYMBOL_ID_NONE`] = unconfigured.
    pub sym_yes: [SymbolId; BIN15_MAX_FAMILIES],
    /// No slot per family.
    pub sym_no: [SymbolId; BIN15_MAX_FAMILIES],
    /// Underlying index per family, into [`Bin15Params::underlying_sym`].
    pub family_underlying: [u8; BIN15_MAX_FAMILIES],
    /// [`FAMILY_OUT_15M`] / [`FAMILY_NATIVE_DAILY`] per family.
    pub family_kind: [u8; BIN15_MAX_FAMILIES],
    /// Configured families.
    pub n_families: usize,
    /// The mark source per distinct underlying.
    pub underlying_sym: [SymbolId; BIN15_MAX_UNDERLYINGS],
    /// Configured underlyings.
    pub n_underlyings: usize,
    /// Arm A edge ×1e6.
    pub e_take_1e6: i64,
    /// Arm B half-spread ×1e6.
    pub h_quote_1e6: i64,
    /// τ floor for a take, ns (per family kind).
    pub tau_min_take_ns: [u64; 2],
    /// τ floor for a quote, ns (per family kind).
    pub tau_min_quote_ns: [u64; 2],
    /// The last window before expiry in which NOTHING is taken, ns.
    pub tail_refuse_ns: u64,
    /// Order TTL, ns — Arm A's deadline and Arm B's quote life.
    pub requote_ttl_ns: u64,
    /// Re-emit an Arm B quote when `|Δp̂|` reaches this ×1e6.
    pub requote_thr_1e6: i64,
    /// Per-order clip ×1e6 contracts.
    pub clip_qty_1e6: i64,
    /// Notional cap per instance ×1e6.
    pub cap_instance_usd_1e6: i64,
    /// Notional cap per UTC day ×1e6.
    pub cap_day_usd_1e6: i64,
    /// BIN15 O9: the COVERAGE-ENTRY notional ×1e6 USD. `0` = off.
    ///
    /// When positive, a 15 m family takes a position on EVERY instance
    /// at this notional, on whichever side `p̂` prefers, WITHOUT
    /// requiring the touch to offer `e_take` of edge. See
    /// [`Bin15Strategy::arm_take`]'s coverage block for why.
    pub entry_usd_1e6: i64,
    /// `1` = Arm B on.
    pub maker_enabled: u8,
    /// `1` = alternate model / null arm by instance parity.
    pub null_arm: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 6],
    /// `ln σ̂` offset per UTC hour ×1e9; all zero = no hour table.
    pub hour_ln_off_1e9: [i64; 24],
    /// Variance-ratio scale on σ̂ ×1e9; `1e9` = no scaling.
    pub scale_1e9: i64,
}

impl Default for Bin15Params {
    fn default() -> Self {
        Self {
            sym_yes: [SYMBOL_ID_NONE; BIN15_MAX_FAMILIES],
            sym_no: [SYMBOL_ID_NONE; BIN15_MAX_FAMILIES],
            family_underlying: [0; BIN15_MAX_FAMILIES],
            family_kind: [FAMILY_OUT_15M; BIN15_MAX_FAMILIES],
            n_families: 0,
            underlying_sym: [SYMBOL_ID_NONE; BIN15_MAX_UNDERLYINGS],
            n_underlyings: 0,
            e_take_1e6: 30_000,
            h_quote_1e6: 25_000,
            tau_min_take_ns: [60_000_000_000, 3_600_000_000_000],
            tau_min_quote_ns: [120_000_000_000, 3_600_000_000_000],
            tail_refuse_ns: 10_000_000_000,
            requote_ttl_ns: 1_000_000_000,
            requote_thr_1e6: 5_000,
            clip_qty_1e6: 500_000_000,
            cap_instance_usd_1e6: 1_000_000_000,
            cap_day_usd_1e6: 5_000_000_000,
            entry_usd_1e6: 0,
            maker_enabled: 1,
            null_arm: 1,
            _pad: [0; 6],
            hour_ln_off_1e9: [0; 24],
            scale_1e9: 1_000_000_000,
        }
    }
}

/// The BIN15 member.
#[repr(C, align(64))]
pub struct Bin15Strategy {
    /// Configuration, fixed at boot.
    params: Bin15Params,
    /// Per-family state.
    fam: [FamilyState; BIN15_MAX_FAMILIES],
    /// Per-underlying marks and forecasts.
    marks: [MarkState; BIN15_MAX_UNDERLYINGS],
    /// The minute grid the close law rolls on.
    bar: BarClock,
    /// The lookup tables.
    lut: Box<price::Bin15Luts>,
    /// Observability.
    counters: Bin15Counters,
    /// Notional booked against `cap_day_usd_1e6` ×1e6.
    day_notional_1e6: i64,
    /// The UTC day the day cap is counted in (wall ns / `DAY_NS`).
    day_epoch: u64,
    /// Next `client_oid` sequence bits.
    oid_seq: u64,
    /// `StrategyCounters::orders_emitted`.
    orders_emitted: u64,
    /// `StrategyCounters::orders_dropped`.
    orders_dropped: u64,
    /// `1` once `configure` succeeded.
    configured: u8,
    /// The regime label set the set stamps; carried, never consulted
    /// (the HORIZON law).
    regime_label: core_types::RegimeLabelSet,
}

impl Default for Bin15Strategy {
    fn default() -> Self {
        Self::new()
    }
}

impl Bin15Strategy {
    /// An unconfigured member. Allocates the LUT box and the eight
    /// forecast engines — BOOT ONLY, and the only heap this crate
    /// touches for the life of the process.
    #[must_use]
    pub fn new() -> Self {
        Self {
            params: Bin15Params::default(),
            fam: [FamilyState::default(); BIN15_MAX_FAMILIES],
            marks: core::array::from_fn(|_| MarkState::new()),
            bar: BarClock::new(core_time::WallAnchor::new(0, 0), 60_000_000_000, 0),
            lut: Box::new(price::Bin15Luts::identity()),
            counters: Bin15Counters::default(),
            day_notional_1e6: 0,
            day_epoch: 0,
            oid_seq: 0,
            orders_emitted: 0,
            orders_dropped: 0,
            configured: 0,
            regime_label: core_types::RegimeLabelSet::default(),
        }
    }

    /// Configure the member (boot only).
    ///
    /// Refuses rather than clamps: a family with no Yes slot, an
    /// underlying index past the configured marks, or a scale of zero
    /// are all operator errors, and a member that quietly ran on
    /// half a config is how a lane comes to trade a universe nobody
    /// chose.
    pub fn configure(
        &mut self,
        params: Bin15Params,
        luts: Box<price::Bin15Luts>,
        anchor: core_time::WallAnchor,
    ) -> Result<(), StrategyError> {
        if params.n_families == 0 || params.n_families > BIN15_MAX_FAMILIES {
            return Err(StrategyError::Config("bin15: family count out of range"));
        }
        if params.n_underlyings == 0 || params.n_underlyings > BIN15_MAX_UNDERLYINGS {
            return Err(StrategyError::Config("bin15: underlying count out of range"));
        }
        if params.scale_1e9 <= 0 {
            return Err(StrategyError::Config("bin15: scale_1e9 must be positive"));
        }
        if params.clip_qty_1e6 <= 0
            || params.cap_instance_usd_1e6 <= 0
            || params.cap_day_usd_1e6 <= 0
        {
            return Err(StrategyError::Config("bin15: a cap or clip is not positive"));
        }
        if params.entry_usd_1e6 < 0 {
            return Err(StrategyError::Config("bin15: entry_usd_1e6 is negative"));
        }
        if params.entry_usd_1e6 > params.cap_instance_usd_1e6 {
            return Err(StrategyError::Config(
                "bin15: entry_usd_1e6 over the per-instance cap — every entry would be clipped",
            ));
        }
        if params.e_take_1e6 < GRID_TICK_1E6 {
            return Err(StrategyError::Config("bin15: e_take under one tick"));
        }
        if params.requote_ttl_ns == 0 {
            return Err(StrategyError::Config("bin15: requote_ttl_ns is zero"));
        }
        let mut u = 0usize;
        while u < params.n_underlyings {
            if params.underlying_sym[u] == SYMBOL_ID_NONE {
                return Err(StrategyError::Config("bin15: underlying with no mark sym"));
            }
            u += 1;
        }
        let mut f = 0usize;
        while f < params.n_families {
            if params.sym_yes[f] == SYMBOL_ID_NONE || params.sym_no[f] == SYMBOL_ID_NONE {
                return Err(StrategyError::Config("bin15: family with no slot pair"));
            }
            if params.family_underlying[f] as usize >= params.n_underlyings {
                return Err(StrategyError::Config("bin15: family underlying out of range"));
            }
            if params.family_kind[f] > FAMILY_NATIVE_DAILY {
                return Err(StrategyError::Config("bin15: unknown family kind"));
            }
            let k = params.family_kind[f] as usize;
            if params.tau_min_take_ns[k] >= tau_of_kind(params.family_kind[f])
                || params.tau_min_quote_ns[k] >= tau_of_kind(params.family_kind[f])
            {
                return Err(StrategyError::Config("bin15: tau_min not under the tenor"));
            }
            f += 1;
        }
        self.params = params;
        self.lut = luts;
        self.bar = BarClock::new(anchor, 60_000_000_000, 0);
        let mut i = 0usize;
        while i < params.n_families {
            self.fam[i].sym_yes = params.sym_yes[i];
            self.fam[i].sym_no = params.sym_no[i];
            self.fam[i].underlying = params.family_underlying[i];
            self.fam[i].kind = params.family_kind[i];
            i += 1;
        }
        self.configured = 1;
        self.refresh_dormant();
        Ok(())
    }

    /// Seed one underlying's forecast window and fitted pairs (boot
    /// only) — the `R` and `P` rows of `bin15-seed-<COIN>.tsv`, in the
    /// VRP seed grammar. Both tenors of the underlying get the same
    /// returns, because the underlying has one price series.
    pub fn seed_returns(&mut self, underlying: usize, rets: &[(u64, i64)]) {
        if underlying >= BIN15_MAX_UNDERLYINGS {
            debug_assert!(false, "seed for an unconfigured underlying");
            return;
        }
        let mut k = 0usize;
        while k < 2 {
            let mut i = 0usize;
            while i < rets.len() {
                let (ts_ms, r) = rets[i];
                self.marks[underlying].vol[k].seed_return(r, ts_ms);
                i += 1;
            }
            k += 1;
        }
    }

    /// Seed one underlying's fitted pairs for ONE tenor (boot only).
    pub fn seed_pairs(&mut self, underlying: usize, kind: u8, pairs: &[(u64, i64, i64)]) {
        if underlying >= BIN15_MAX_UNDERLYINGS || kind > FAMILY_NATIVE_DAILY {
            debug_assert!(false, "seed pairs for an unconfigured slot");
            return;
        }
        let k = kind as usize;
        let mut i = 0usize;
        while i < pairs.len() {
            let (ts_ms, x, y) = pairs[i];
            self.marks[underlying].vol[k].seed_pair_at(ts_ms, x, y);
            i += 1;
        }
    }

    /// The counters.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> Bin15Counters {
        self.counters
    }

    /// Read one family's state (observability and tests).
    #[inline]
    #[must_use]
    pub fn family(&self, idx: usize) -> Option<&FamilyState> {
        if idx >= self.params.n_families {
            None
        } else {
            Some(&self.fam[idx])
        }
    }

    /// Configured families.
    #[inline]
    #[must_use]
    pub const fn n_families(&self) -> usize {
        self.params.n_families
    }

    /// The params in force.
    #[inline]
    #[must_use]
    pub const fn params(&self) -> &Bin15Params {
        &self.params
    }

    /// Recount the dormant families. A LEVEL, not a total: an operator
    /// reading `families_dormant = 3` learns that three configured
    /// slots have no instance right now, which is the normal state
    /// between a settle and a create and an alarm if it sticks.
    fn refresh_dormant(&mut self) {
        let mut n = 0u64;
        let mut i = 0usize;
        while i < self.params.n_families {
            if !self.fam[i].is_live() {
                n += 1;
            }
            i += 1;
        }
        self.counters.families_dormant = n;
    }

    /// The family whose Yes or No slot is `sym`, and which leg it is.
    #[inline]
    fn family_of_sym(&self, sym: SymbolId) -> Option<(usize, bool)> {
        let mut i = 0usize;
        while i < self.params.n_families {
            if self.fam[i].sym_yes == sym {
                return Some((i, true));
            }
            if self.fam[i].sym_no == sym {
                return Some((i, false));
            }
            i += 1;
        }
        None
    }

    /// The underlying index whose mark source is `sym`.
    #[inline]
    fn underlying_of_sym(&self, sym: SymbolId) -> Option<usize> {
        let mut i = 0usize;
        while i < self.params.n_underlyings {
            if self.params.underlying_sym[i] == sym {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// The family index carrying outcome `outcome`.
    #[inline]
    fn family_of_outcome(&self, outcome: u32) -> Option<usize> {
        let mut i = 0usize;
        while i < self.params.n_families {
            if self.fam[i].live.outcome == outcome {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

/// The `core-vol` tenor a family kind prices on.
#[inline(always)]
#[must_use]
pub const fn tau_of_kind(kind: u8) -> u64 {
    if kind == FAMILY_NATIVE_DAILY {
        TAU_8H_NS
    } else {
        TAU_15M_NS
    }
}

/// The unit bridge between `core-vol` and the pricer: `1e8`.
///
/// `core-vol` reports volatility in RAW BPS ×1e9 — every number in that
/// domain is built from `ret_bps_1e9`, and one bp is `1e-4`, so a
/// bps×1e9 value is a **fraction ×1e13**. `price::fair_value` wants a
/// per-minute variance of log-price as a **fraction² ×1e18**. Squaring
/// a fraction×1e13 gives a fraction²×1e26, so the square comes down by
/// `1e26 / 1e18 = 1e8`.
///
/// This was missing until O4b's harness arm went looking for a fair
/// value it could predict, and the way it failed is worth writing down:
/// σ̂ came out `1e4` too large, so every `d` collapsed toward zero and
/// the member priced EVERY binary at almost exactly 0.5 — while
/// `reprices`, `takes_submitted` and `takes_filled` all climbed exactly
/// as they would if it were working. A member that prices every
/// instrument at a coin flip and then buys whatever is offered under
/// 0.47 is not the model; it is a spread harvester with a model-shaped
/// counter set. Nothing short of predicting the number catches that.
const BPS2_TO_FRAC2_1E8: i128 = 100_000_000;

/// Minutes in the tenor a family kind prices on.
#[inline(always)]
#[must_use]
const fn tau_minutes_of_kind(kind: u8) -> i128 {
    if kind == FAMILY_NATIVE_DAILY {
        TAU_8H_MINUTES
    } else {
        TAU_15M_MINUTES
    }
}

// ---------------------------------------------------------------
// the roll, the mark and the minute
// ---------------------------------------------------------------

impl Bin15Strategy {
    /// Bind a new instance to a family slot.
    fn bind(&mut self, idx: usize, spec: BinarySpec) {
        debug_assert!(idx < self.params.n_families);
        debug_assert!(spec.outcome != 0);
        self.fam[idx].clear_instance();
        self.fam[idx].live = spec;
        // The null arm alternates by instance parity, so a session's
        // model and null samples are interleaved in time rather than
        // split into two epochs the market itself could differ across.
        self.fam[idx].arm = if self.params.null_arm == 1 {
            (spec.outcome & 1) as u8
        } else {
            ARM_MODEL
        };
        // Arm the forecast hold for THIS instance's horizon. 15 m only:
        // a daily family's 8 h engine is armed by its own 8 h grid, not
        // by a 24 h instance boundary, and arming it here would hand
        // the 8 h fit a 24 h realised window.
        if self.fam[idx].kind == FAMILY_OUT_15M {
            let u = self.fam[idx].underlying as usize;
            let expiry_ms = spec.expiry_ns / 1_000_000;
            // IV-optional (core-vol O4a): a HIP-4 outcome market has no
            // implied vol, so the hold forms its pair and scores no
            // QLIKE row. That is the law, not a missing argument.
            self.marks[u].vol[FAMILY_OUT_15M as usize].arm_hold_at(expiry_ms, TAU_15M_NS, 0);
        }
        self.counters.rolls = self.counters.rolls.wrapping_add(1);
        self.refresh_dormant();
    }

    /// Settle a family's live instance.
    fn settle(&mut self, idx: usize) {
        debug_assert!(idx < self.params.n_families);
        if !self.fam[idx].is_live() {
            return;
        }
        if self.fam[idx].kind == FAMILY_OUT_15M {
            let u = self.fam[idx].underlying as usize;
            let k = FAMILY_OUT_15M as usize;
            if let Some(rv) = self.marks[u].vol[k].realised_since_arm_1e9() {
                self.marks[u].vol[k].observe_settlement(rv);
            } else {
                // No window ⇒ no pair. `core-vol` ignores a settlement
                // with nothing armed, but an armed hold whose window
                // never closed must be DROPPED rather than paired with
                // a zero — a fabricated `y` is a fabricated fit.
                self.marks[u].vol[k].disarm();
            }
        }
        self.fam[idx].clear_instance();
        self.counters.rolls_settled = self.counters.rolls_settled.wrapping_add(1);
        self.refresh_dormant();
    }

    /// One `InstrumentRoll` event (`ChannelId::InstrumentRoll = 13`).
    ///
    /// `venue_seq` packs the identity exactly as the ingress writes it
    /// and `claude_worker.hip4.unpack_roll_seq` reads it: bits 0..32
    /// outcome, 32..48 TWAP seconds, 48..56 family index, 56..64
    /// 0 = created / 1 = settled. `v0` is the strike ×1e6 and `v1` the
    /// expiry ns. The FAMILY INDEX is the ingress's, and this member's
    /// family order is the same boot list, so it indexes directly —
    /// but it is bounds-checked, because trusting a wire byte to index
    /// an array is how a malformed frame becomes a panic.
    fn on_roll(&mut self, event: &core_types::ChannelEvent) {
        let seq = event.venue_seq;
        let outcome = (seq & 0xFFFF_FFFF) as u32;
        let twap_s = (seq >> 32) & 0xFFFF;
        let family = ((seq >> 48) & 0xFF) as usize;
        let settled = ((seq >> 56) & 0xFF) as u8;
        if family >= self.params.n_families {
            self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
            return;
        }
        if settled == 1 {
            // The settled row names the instance that is ENDING, and
            // the OUTCOME ID is the stronger identity: the family byte
            // is this member's boot ordering agreeing with the
            // ingress's, while the outcome is the venue's own name for
            // the instrument. So settle the family that actually holds
            // it, wherever the byte points — and settle NOTHING when no
            // family holds it, because a duplicate or reordered frame
            // must never clear the successor that has already bound.
            let target = if self.fam[family].live.outcome == outcome {
                Some(family)
            } else if outcome == 0 {
                // A settled row with no outcome names the slot alone.
                Some(family)
            } else {
                self.family_of_outcome(outcome)
            };
            match target {
                Some(f) => self.settle(f),
                None => {
                    self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
                }
            }
            return;
        }
        if outcome == 0 || event.v0 <= 0 || event.v1 <= 0 {
            self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
            return;
        }
        self.bind(
            family,
            BinarySpec {
                outcome,
                _pad: 0,
                strike_1e6: event.v0,
                expiry_ns: event.v1 as u64,
                twap_ns: twap_s.saturating_mul(1_000_000_000),
                created_ns: event.ts_ns,
            },
        );
    }

    /// One `Mark` event for a configured underlying.
    ///
    /// Two jobs, in order: publish the mark so the pricer can run, and
    /// roll the minute grid when the boundary is crossed. The CLOSE of
    /// a minute is its last mark, so the roll publishes the value
    /// carried across the boundary rather than the first print of the
    /// new minute — the same law `strategy-vrp` applies to its
    /// underlying mid.
    fn on_mark(&mut self, u: usize, mark_1e6: i64, now: NsTs) {
        debug_assert!(u < self.params.n_underlyings);
        if mark_1e6 <= 0 {
            return;
        }
        let minute = self.bar.bar_id(now);
        let prev_mark = self.marks[u].mark_1e6;
        let prev_minute = self.marks[u].minute_id;
        if prev_minute == 0 {
            self.marks[u].minute_id = minute;
        } else if minute != prev_minute {
            let close_ms = prev_minute.saturating_mul(60_000);
            let mut k = 0usize;
            while k < 2 {
                self.marks[u].vol[k].on_minute_close_at(prev_mark, close_ms);
                k += 1;
            }
            self.marks[u].minute_id = minute;
            // Transcendentals ONCE per minute per underlying. This is
            // the crate's whole reason for a per-minute variance
            // instead of a per-tick one.
            self.refresh_sigma(u, self.bar.anchor.wall_of(now));
        }
        self.marks[u].mark_1e6 = mark_1e6;
        self.marks[u].ts_ns = now;
    }

    /// Recompute one underlying's per-minute variance, both tenors.
    ///
    /// `ln σ̂` comes out of `core-vol`, the hour offset is added IN THE
    /// LOG DOMAIN (where a multiplicative correction belongs), the
    /// result is exponentiated once, scaled by the variance ratio, and
    /// squared into a per-minute variance by dividing by the tenor's
    /// own minutes. Nothing here runs per tick.
    fn refresh_sigma(&mut self, u: usize, wall_ns: u64) {
        let hour = ((wall_ns / 3_600_000_000_000) % 24) as usize;
        let off = self.params.hour_ln_off_1e9[hour];
        let scale = self.params.scale_1e9;
        let mut k = 0usize;
        while k < 2 {
            let tau = tau_of_kind(k as u8);
            let sig2 = match self.marks[u].vol[k].ln_sigma_hat_1e9(tau) {
                Some(ln) => {
                    let sig = core_vol::fx::exp_1e9(ln.saturating_add(off));
                    if sig == 0 || sig == u64::MAX {
                        0
                    } else {
                        let scaled = (sig as i128 * scale as i128) / 1_000_000_000;
                        // σ_τ² / τ_minutes = per-minute variance — but
                        // in the PRICER's units, which are not
                        // `core-vol`'s. See [`BPS2_TO_FRAC2_1E8`].
                        (scaled * scaled)
                            / (BPS2_TO_FRAC2_1E8 * tau_minutes_of_kind(k as u8))
                    }
                }
                None => 0,
            };
            self.marks[u].sig2_min_1e18[k] = sig2;
            k += 1;
        }
    }

    /// One `SetBinarySpec` command (`AiCmdKind::SetBinarySpec = 13`).
    ///
    /// The wire gate (`AiCmd::validate_shape`) has already checked
    /// everything checkable without a clock. What is left is the clock
    /// itself and the slot's existence, both of which are this
    /// member's: `qty > now` in WALL ns, and a `sym` that names a
    /// configured Yes slot.
    fn on_spec(&mut self, cmd: &core_types::AiCmd, now: NsTs) {
        let Some((idx, is_yes)) = self.family_of_sym(cmd.sym) else {
            self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
            return;
        };
        if !is_yes {
            // The sym NAMES the slot pair, and the Yes leg is its name.
            self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
            return;
        }
        if cmd.flags & core_types::AI_CMD_FLAG_CLEAR_SPEC != 0 {
            self.settle(idx);
            self.counters.spec_overrides = self.counters.spec_overrides.wrapping_add(1);
            return;
        }
        let wall = self.bar.anchor.wall_of(now);
        if cmd.qty <= 0 || (cmd.qty as u64) <= wall {
            self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
            return;
        }
        if cmd.px < 1_000_000 || cmd.px > 10_000_000_000_000 {
            self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
            return;
        }
        if cmd.param_id > 3_600 || cmd.ttl_ns == 0 || cmd.ttl_ns > u64::from(u32::MAX) {
            self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
            return;
        }
        self.bind(
            idx,
            BinarySpec {
                outcome: cmd.ttl_ns as u32,
                _pad: 0,
                strike_1e6: cmd.px,
                expiry_ns: cmd.qty as u64,
                twap_ns: u64::from(cmd.param_id).saturating_mul(1_000_000_000),
                created_ns: now,
            },
        );
        self.counters.spec_overrides = self.counters.spec_overrides.wrapping_add(1);
    }
}

// ---------------------------------------------------------------
// the re-price and the two arms
// ---------------------------------------------------------------

/// Why a re-price produced nothing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Hold {
    /// No live instance on this slot.
    Dormant,
    /// Inside `tail_refuse_ns`, or past expiry.
    Tail,
    /// The underlying mark or the forecast is absent.
    Stale,
}

impl Bin15Strategy {
    /// Re-price one family, then let the arms act on the result.
    ///
    /// Called on every `Mark` of the family's underlying and on every
    /// touch change of either leg — the two things that can move the
    /// decision. Never on a timer: a re-price with no new evidence is
    /// the same number.
    fn reprice<C: Ctx>(&mut self, ctx: &mut C, idx: usize, now: NsTs) {
        let Err(reason) = self.try_reprice(ctx, idx, now) else {
            return;
        };
        match reason {
            Hold::Dormant => {}
            Hold::Tail => self.counters.skipped_tail = self.counters.skipped_tail.wrapping_add(1),
            Hold::Stale => {
                self.counters.skipped_stale = self.counters.skipped_stale.wrapping_add(1);
            }
        }
    }

    /// The pricer proper. `Err` names the gate that held.
    fn try_reprice<C: Ctx>(&mut self, ctx: &mut C, idx: usize, now: NsTs) -> Result<(), Hold> {
        debug_assert!(idx < self.params.n_families);
        if !self.fam[idx].is_live() {
            return Err(Hold::Dormant);
        }
        let wall = self.bar.anchor.wall_of(now);
        let expiry = self.fam[idx].live.expiry_ns;
        // Past expiry, or inside the tail: the venue clears the book at
        // expiry and the model's own error near it exceeds any edge.
        if wall.saturating_add(self.params.tail_refuse_ns) >= expiry {
            return Err(Hold::Tail);
        }
        let u = self.fam[idx].underlying as usize;
        let k = self.fam[idx].kind as usize;
        let mark = self.marks[u].mark_1e6;
        let sig2 = self.marks[u].sig2_min_1e18[k];
        if mark <= 0 || sig2 <= 0 {
            return Err(Hold::Stale);
        }
        // τ is time to expiry PLUS a third of the settlement TWAP
        // window: a TWAP-settled binary is not decided at `T` but
        // averaged over `[T, T + twap]`, and a Brownian average over a
        // window carries a third of that window's variance.
        let tau_ns = (expiry - wall).saturating_add(self.fam[idx].live.twap_ns / 3);
        let Some(fair) = price::fair_value(
            &self.lut,
            mark,
            self.fam[idx].live.strike_1e6,
            tau_ns,
            sig2,
        ) else {
            return Err(Hold::Stale);
        };
        self.fam[idx].p_hat_1e6 = fair.p_hat_1e6;
        self.fam[idx].p_raw_1e6 = fair.p_raw_1e6;
        self.fam[idx].p_ts_ns = now;
        // BIN15 O6: every input the fair value was built from, kept
        // for `/metrics`. Four stores on a path that already wrote
        // three; no allocation and no branch.
        self.fam[idx].d_1e6 = fair.d_1e6;
        self.fam[idx].den_1e9 = fair.den_1e9;
        self.fam[idx].mark_1e6 = mark;
        self.fam[idx].last_tau_ns = tau_ns;
        self.counters.reprices = self.counters.reprices.wrapping_add(1);
        self.arm_take(ctx, idx, tau_ns, now);
        if self.params.maker_enabled == 1 {
            self.arm_quote(ctx, idx, tau_ns, now);
        }
        Ok(())
    }

    /// Room left under both notional caps ×1e6, at `px_1e6` per
    /// contract. `0` = no room.
    fn cap_room_1e6(&self, idx: usize, px_1e6: i64) -> i64 {
        if px_1e6 <= 0 {
            return 0;
        }
        let inst = self.params.cap_instance_usd_1e6 - self.fam[idx].notional_instance_1e6;
        let day = self.params.cap_day_usd_1e6 - self.day_notional_1e6;
        let usd = inst.min(day);
        if usd <= 0 {
            return 0;
        }
        // Contracts ×1e6 affordable at this price, floored to a whole
        // contract because the venue's lot is one.
        let qty = (usd as i128 * 1_000_000) / px_1e6 as i128;
        let qty = i64::try_from(qty).unwrap_or(i64::MAX);
        qty - qty.rem_euclid(GRID_LOT_1E6)
    }

    /// Whether a price and size are on the venue's grid. The harness
    /// refuses off-grid orders outright (O3), so an arm that emitted
    /// one would be counted as a submit and never fill — a silent
    /// hole in the member's own accounting.
    #[inline]
    fn on_grid(px_1e6: i64, qty_1e6: i64) -> bool {
        px_1e6 % GRID_TICK_1E6 == 0
            && qty_1e6 % GRID_LOT_1E6 == 0
            && (GRID_PX_MIN_1E6..=GRID_PX_MAX_1E6).contains(&px_1e6)
            && (px_1e6 as i128 * qty_1e6 as i128) / 1_000_000 >= GRID_MIN_NOTIONAL_1E6
    }

    /// Arm A — the taker.
    ///
    /// Three shapes, and only three. BUY Yes when the Yes ask is below
    /// fair by the edge; BUY No when the No ask is below `1 − p̂` by the
    /// edge (never "sell Yes": a HIP-4 position cannot go short, and
    /// the No side is the vehicle); and CLOSE inventory when the bid on
    /// a side we hold is RICH by the edge. The close is the only sell
    /// this member ever emits.
    fn arm_take<C: Ctx>(&mut self, ctx: &mut C, idx: usize, tau_ns: u64, now: NsTs) {
        let kind = self.fam[idx].kind as usize;
        if tau_ns < self.params.tau_min_take_ns[kind] {
            self.counters.skipped_tau = self.counters.skipped_tau.wrapping_add(1);
            return;
        }
        // One pending take per family: a second would double the size
        // the caps just approved.
        if self.fam[idx].pend_take.live() {
            return;
        }
        let p_hat = self.fam[idx].p_hat_1e6;
        let e = self.params.e_take_1e6;
        let yes = self.fam[idx].touch_yes;
        let no = self.fam[idx].touch_no;
        if !yes.actionable() || !no.actionable() {
            self.counters.skipped_book = self.counters.skipped_book.wrapping_add(1);
            return;
        }
        // A closing take first: reducing risk outranks adding it.
        if self.fam[idx].pos_yes_1e6 > 0 && yes.touch.bid_1e6 >= p_hat.saturating_add(e) {
            let qty = self.fam[idx].pos_yes_1e6.min(yes.touch.bid_qty_1e6);
            self.emit_take(ctx, idx, true, Side::Ask, yes.touch.bid_1e6, qty, now, true);
            return;
        }
        if self.fam[idx].pos_no_1e6 > 0
            && no.touch.bid_1e6 >= (ONE_1E6 - p_hat).saturating_add(e)
        {
            let qty = self.fam[idx].pos_no_1e6.min(no.touch.bid_qty_1e6);
            self.emit_take(ctx, idx, false, Side::Ask, no.touch.bid_1e6, qty, now, true);
            return;
        }
        // BIN15 O9 — the COVERAGE ENTRY (operator ruling 2026-09-13).
        //
        // A 15 m family takes a position on EVERY instance, at a fixed
        // `entry_usd_1e6` notional, on whichever side `p̂` prefers, and
        // WITHOUT requiring the touch to offer `e_take` of edge. The
        // point is evidence, not edge: of the 33 instances in the
        // 2026-09-12 capture, 22 drew no order at all, so two thirds of
        // the calibration ledger was never written and the G6.1 gate
        // (200 settled instances per phase) could not accrue. P&L under
        // this rule is a CALIBRATION DIAGNOSTIC, not a strategy result.
        //
        // `notional_instance_1e6 == 0` is the once-per-instance test:
        // `clear_instance` zeroes it at every roll, so the entry fires
        // on the first actionable reprice of each instance and never
        // again. The caps still bind — an entry that cannot fit under
        // them is counted `skipped_cap` like any other.
        if self.params.entry_usd_1e6 > 0
            && self.fam[idx].kind == FAMILY_OUT_15M
            && self.fam[idx].covered == 0
        {
            let want_yes = p_hat >= ONE_1E6 / 2;
            let touch = if want_yes { yes } else { no };
            let px = touch.touch.ask_1e6;
            // Contracts ×1e6 the entry notional buys at that ask, then
            // bounded by the resting size and by the caps' own room.
            let want = (self.params.entry_usd_1e6 as i128 * 1_000_000) / px as i128;
            let want = i64::try_from(want).unwrap_or(i64::MAX);
            let qty = want
                .min(touch.touch.ask_qty_1e6)
                .min(self.cap_room_1e6(idx, px));
            // Marked BEFORE the emit: a coverage entry the caps or the
            // grid refuse is still this instance's one attempt, and
            // retrying it on every reprice would spray the book.
            self.fam[idx].covered = 1;
            self.emit_take(ctx, idx, want_yes, Side::Bid, px, qty, now, false);
            return;
        }
        // Then the opening takes.
        if yes.touch.ask_1e6.saturating_add(e) <= p_hat {
            let room = self.cap_room_1e6(idx, yes.touch.ask_1e6);
            let qty = yes.touch.ask_qty_1e6.min(self.params.clip_qty_1e6).min(room);
            self.emit_take(ctx, idx, true, Side::Bid, yes.touch.ask_1e6, qty, now, false);
            return;
        }
        if no.touch.ask_1e6.saturating_add(e) <= ONE_1E6 - p_hat {
            let room = self.cap_room_1e6(idx, no.touch.ask_1e6);
            let qty = no.touch.ask_qty_1e6.min(self.params.clip_qty_1e6).min(room);
            self.emit_take(ctx, idx, false, Side::Bid, no.touch.ask_1e6, qty, now, false);
        }
    }

    /// Arm B — the paper maker.
    ///
    /// The BID side is the primary quote. The ASK side quotes only up
    /// to the inventory we can prove we hold, because an ask with no
    /// inventory is a short the venue refuses and the paper model would
    /// happily fill. That asymmetry is why Arm B's paper P&L is a LOWER
    /// BOUND of a real two-sided maker's, and it is stated in the
    /// module header for the same reason.
    fn arm_quote<C: Ctx>(&mut self, ctx: &mut C, idx: usize, tau_ns: u64, now: NsTs) {
        let kind = self.fam[idx].kind as usize;
        if tau_ns < self.params.tau_min_quote_ns[kind] {
            self.counters.skipped_tau = self.counters.skipped_tau.wrapping_add(1);
            return;
        }
        let yes = self.fam[idx].touch_yes;
        if !yes.actionable() {
            self.counters.skipped_book = self.counters.skipped_book.wrapping_add(1);
            return;
        }
        // The null arm quotes around the venue's OWN mid: if the venue
        // mid earns the same as the model, the model is not the edge.
        let centre = if self.fam[idx].arm == ARM_NULL {
            (yes.touch.bid_1e6 + yes.touch.ask_1e6) >> 1
        } else {
            // Skew against inventory: one clip of Yes moves the quote
            // by one half-spread, so a filled maker stops re-offering
            // at the price that filled it.
            let inv = self.fam[idx].pos_yes_1e6;
            let skew = if self.params.clip_qty_1e6 == 0 {
                0
            } else {
                (inv as i128 * self.params.h_quote_1e6 as i128
                    / self.params.clip_qty_1e6 as i128) as i64
            };
            self.fam[idx].p_hat_1e6 - skew
        };
        let h = self.params.h_quote_1e6;
        let mut side_idx = 0usize;
        while side_idx < QUOTE_SIDES {
            let is_bid = side_idx == 0;
            // TWO gates, and both are load-bearing.
            //
            // The first is forced: this member has no cancel path
            // (Stage-3), so a live quote can only be replaced by
            // letting it expire. `requote_ttl_ns` is therefore the
            // replace cadence, not a nicety.
            //
            // The second is the one that stops a re-quote loop. A
            // quote that reached its TTL unfilled has just told us that
            // price does not fill in this book; re-offering the SAME
            // price a second later adds no information and would emit
            // once per TTL forever. So after an expiry we quote again
            // only when the fair value has actually moved by
            // `requote_thr_1e6` (or when we have never quoted this
            // side, where `last_quote_p` is 0 and the test passes).
            if self.fam[idx].pend_quote[side_idx].live() {
                side_idx += 1;
                continue;
            }
            let last = self.fam[idx].last_quote_p_1e6[side_idx];
            if last != 0 && (centre - last).abs() < self.params.requote_thr_1e6 {
                side_idx += 1;
                continue;
            }
            let px = if is_bid {
                // Never cross the touch: a maker that crosses is a
                // taker paying the spread it meant to earn.
                price::floor_grid_1e6(centre - h, GRID_TICK_1E6).min(yes.touch.bid_1e6)
            } else {
                price::ceil_grid_1e6(centre + h, GRID_TICK_1E6).max(yes.touch.ask_1e6)
            };
            let qty = if is_bid {
                let room = self.cap_room_1e6(idx, px);
                self.params.clip_qty_1e6.min(room)
            } else {
                // The short rule.
                self.params.clip_qty_1e6.min(self.fam[idx].pos_yes_1e6)
            };
            if qty <= 0 {
                if is_bid {
                    self.counters.skipped_cap = self.counters.skipped_cap.wrapping_add(1);
                } else {
                    self.counters.skipped_inventory =
                        self.counters.skipped_inventory.wrapping_add(1);
                }
                side_idx += 1;
                continue;
            }
            let side = if is_bid { Side::Bid } else { Side::Ask };
            self.emit_quote(ctx, idx, side_idx, side, px, qty, centre, now);
            side_idx += 1;
        }
    }

    /// The `client_oid` of one intent. Bit 63 = arm, bit 48 = side,
    /// bits 32..48 = family, bits 0..32 = outcome. The sequence rides
    /// in the unused bits 49..63 so two intents on the same leg of the
    /// same instance are still distinct.
    fn next_oid(&mut self, idx: usize, is_yes: bool) -> u64 {
        self.oid_seq = self.oid_seq.wrapping_add(1);
        let arm = u64::from(self.fam[idx].arm & 1) << OID_ARM_SHIFT;
        let side = u64::from(!is_yes) << OID_SIDE_SHIFT;
        let fam = (idx as u64 & 0xFFFF) << OID_FAMILY_SHIFT;
        let seq = (self.oid_seq & 0x3FFF) << 49;
        arm | seq | side | fam | (u64::from(self.fam[idx].live.outcome) & OID_OUTCOME_MASK)
    }
}

// ---------------------------------------------------------------
// emission, fills and the pending sweep
// ---------------------------------------------------------------

impl Bin15Strategy {
    /// Submit one order, book the pending, count the outcome.
    ///
    /// `Order::strategy_id` is left alone: the strategy-set's stamping
    /// ctx writes the slot (M4.1 M-c), and a member that stamped
    /// itself would disagree with the set the moment a slot moved.
    #[allow(clippy::too_many_arguments)]
    fn submit<C: Ctx>(
        &mut self,
        ctx: &mut C,
        sym: SymbolId,
        side: Side,
        kind: u8,
        px_1e6: i64,
        qty_1e6: i64,
        oid: u64,
        now: NsTs,
    ) -> bool {
        debug_assert!(px_1e6 > 0 && qty_1e6 > 0);
        let order = Order::new(
            now,
            VenueId::Hyperliquid,
            sym,
            side,
            kind,
            Price::from_raw(px_1e6),
            Qty::from_raw(qty_1e6),
            oid,
        )
        .with_ttl_ns(self.params.requote_ttl_ns);
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

    /// Emit one Arm A IoC.
    #[allow(clippy::too_many_arguments)]
    fn emit_take<C: Ctx>(
        &mut self,
        ctx: &mut C,
        idx: usize,
        is_yes: bool,
        side: Side,
        px_1e6: i64,
        qty_1e6: i64,
        now: NsTs,
        closing: bool,
    ) {
        if qty_1e6 <= 0 {
            if closing {
                self.counters.skipped_inventory =
                    self.counters.skipped_inventory.wrapping_add(1);
            } else {
                self.counters.skipped_cap = self.counters.skipped_cap.wrapping_add(1);
            }
            return;
        }
        let qty = qty_1e6 - qty_1e6.rem_euclid(GRID_LOT_1E6);
        if !Self::on_grid(px_1e6, qty) {
            self.counters.skipped_grid = self.counters.skipped_grid.wrapping_add(1);
            return;
        }
        let sym = if is_yes {
            self.fam[idx].sym_yes
        } else {
            self.fam[idx].sym_no
        };
        let oid = self.next_oid(idx, is_yes);
        if !self.submit(ctx, sym, side, ORDER_KIND_IOC, px_1e6, qty, oid, now) {
            return;
        }
        self.fam[idx].pend_take = PendingLeg {
            oid,
            deadline_ns: now.saturating_add(self.params.requote_ttl_ns),
            px_1e6,
            qty_1e6: qty,
            filled_1e6: 0,
            is_yes: u8::from(is_yes),
            side: side as u8,
            _pad: [0; 6],
        };
        // An OPENING take books its notional against both caps at
        // SUBMIT, not at fill. A cap that only counted fills would let
        // a member with eight unfilled intents in flight commit eight
        // times its limit, which is the one thing a cap exists to stop.
        if !closing {
            let notional = ((px_1e6 as i128 * qty as i128) / 1_000_000) as i64;
            self.fam[idx].notional_instance_1e6 =
                self.fam[idx].notional_instance_1e6.saturating_add(notional);
            self.day_notional_1e6 = self.day_notional_1e6.saturating_add(notional);
            self.counters.takes_submitted = self.counters.takes_submitted.wrapping_add(1);
        } else {
            self.counters.closes_submitted = self.counters.closes_submitted.wrapping_add(1);
        }
    }

    /// Emit one Arm B resting quote.
    #[allow(clippy::too_many_arguments)]
    fn emit_quote<C: Ctx>(
        &mut self,
        ctx: &mut C,
        idx: usize,
        side_idx: usize,
        side: Side,
        px_1e6: i64,
        qty_1e6: i64,
        centre_1e6: i64,
        now: NsTs,
    ) {
        let qty = qty_1e6 - qty_1e6.rem_euclid(GRID_LOT_1E6);
        if !Self::on_grid(px_1e6, qty) {
            self.counters.skipped_grid = self.counters.skipped_grid.wrapping_add(1);
            return;
        }
        let sym = self.fam[idx].sym_yes;
        let oid = self.next_oid(idx, true);
        if !self.submit(ctx, sym, side, ORDER_KIND_MAKER, px_1e6, qty, oid, now) {
            return;
        }
        self.fam[idx].pend_quote[side_idx] = PendingLeg {
            oid,
            deadline_ns: now.saturating_add(self.params.requote_ttl_ns),
            px_1e6,
            qty_1e6: qty,
            filled_1e6: 0,
            is_yes: 1,
            side: side as u8,
            _pad: [0; 6],
        };
        self.fam[idx].last_quote_p_1e6[side_idx] = centre_1e6;
        if side == Side::Bid {
            let notional = ((px_1e6 as i128 * qty as i128) / 1_000_000) as i64;
            self.fam[idx].notional_instance_1e6 =
                self.fam[idx].notional_instance_1e6.saturating_add(notional);
            self.day_notional_1e6 = self.day_notional_1e6.saturating_add(notional);
        }
        self.counters.quotes_submitted = self.counters.quotes_submitted.wrapping_add(1);
    }

    /// Book one fill against the pending it belongs to.
    ///
    /// POSITIONS COME FROM FILLS. A fill whose `order_id` matches no
    /// pending is counted (`unknown_fills`) and changes nothing — it
    /// means this member's book of intents disagrees with the
    /// dispatcher's, which is a defect to surface, not a position to
    /// invent.
    fn book_fill(&mut self, fill: &core_types::Fill) {
        let qty = fill.qty.raw();
        if qty <= 0 {
            return;
        }
        let mut i = 0usize;
        while i < self.params.n_families {
            if self.fam[i].pend_take.oid == fill.order_id {
                let is_yes = self.fam[i].pend_take.is_yes == 1;
                let buy = self.fam[i].pend_take.side == Side::Bid as u8;
                self.apply_position(i, is_yes, buy, qty);
                self.fam[i].pend_take.filled_1e6 =
                    self.fam[i].pend_take.filled_1e6.saturating_add(qty);
                // An IoC is judged ONCE, so any fill closes the leg.
                self.fam[i].pend_take = PendingLeg::default();
                self.counters.takes_filled = self.counters.takes_filled.wrapping_add(1);
                self.counters.fills = self.counters.fills.wrapping_add(1);
                return;
            }
            let mut sidx = 0usize;
            while sidx < QUOTE_SIDES {
                if self.fam[i].pend_quote[sidx].oid == fill.order_id {
                    let buy = self.fam[i].pend_quote[sidx].side == Side::Bid as u8;
                    self.apply_position(i, true, buy, qty);
                    let filled = self.fam[i].pend_quote[sidx].filled_1e6.saturating_add(qty);
                    self.fam[i].pend_quote[sidx].filled_1e6 = filled;
                    // A maker fills PARTIALLY and keeps resting, so the
                    // leg closes only when it is full.
                    if filled >= self.fam[i].pend_quote[sidx].qty_1e6 {
                        self.fam[i].pend_quote[sidx] = PendingLeg::default();
                    }
                    self.counters.quotes_filled = self.counters.quotes_filled.wrapping_add(1);
                    self.counters.fills = self.counters.fills.wrapping_add(1);
                    return;
                }
                sidx += 1;
            }
            i += 1;
        }
        self.counters.unknown_fills = self.counters.unknown_fills.wrapping_add(1);
    }

    /// Move one leg's position. A buy adds, a sell reduces, and a sell
    /// can never take a side below zero — the venue has no short.
    fn apply_position(&mut self, idx: usize, is_yes: bool, buy: bool, qty_1e6: i64) {
        let pos = if is_yes {
            &mut self.fam[idx].pos_yes_1e6
        } else {
            &mut self.fam[idx].pos_no_1e6
        };
        if buy {
            *pos = pos.saturating_add(qty_1e6);
        } else {
            *pos = (*pos - qty_1e6).max(0);
        }
    }

    /// Write off every pending whose deadline has passed.
    ///
    /// The clock is the only thing that says an unfilled order is a
    /// decision. Without this the member would hold a pending forever
    /// and never re-price that leg — which reads, from the outside,
    /// exactly like a member that has stopped working.
    fn sweep_pendings(&mut self, now: NsTs) {
        let mut i = 0usize;
        while i < self.params.n_families {
            if self.fam[i].pend_take.live() && now >= self.fam[i].pend_take.deadline_ns {
                let unfilled = self.fam[i].pend_take.filled_1e6 == 0;
                self.fam[i].pend_take = PendingLeg::default();
                if unfilled {
                    self.counters.takes_unfilled =
                        self.counters.takes_unfilled.wrapping_add(1);
                }
            }
            let mut s = 0usize;
            while s < QUOTE_SIDES {
                if self.fam[i].pend_quote[s].live() && now >= self.fam[i].pend_quote[s].deadline_ns
                {
                    let unfilled = self.fam[i].pend_quote[s].filled_1e6 == 0;
                    self.fam[i].pend_quote[s] = PendingLeg::default();
                    if unfilled {
                        self.counters.quotes_expired =
                            self.counters.quotes_expired.wrapping_add(1);
                    }
                }
                s += 1;
            }
            i += 1;
        }
    }

    /// Roll the day cap at 00:00Z.
    fn roll_day(&mut self, wall_ns: u64) {
        let epoch = wall_ns / DAY_NS;
        if self.day_epoch == 0 {
            self.day_epoch = epoch;
        } else if epoch != self.day_epoch {
            self.day_epoch = epoch;
            self.day_notional_1e6 = 0;
        }
    }
}

// ---------------------------------------------------------------
// the trait
// ---------------------------------------------------------------

impl StrategyCounters for Bin15Strategy {
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
        "bin15"
    }

    #[inline]
    fn bin15_counters(&self) -> Bin15Counters {
        self.counters
    }

    fn bin15_families_view(&self, out: &mut [strategy_core::Bin15FamilyView]) -> u32 {
        let n = self.params.n_families.min(out.len());
        let mut i = 0usize;
        while i < n {
            out[i] = strategy_core::Bin15FamilyView {
                live_outcome: self.fam[i].live.outcome,
                tau_s: u32::try_from(self.fam[i].last_tau_ns / 1_000_000_000)
                    .unwrap_or(u32::MAX),
                p_hat_1e6: self.fam[i].p_hat_1e6,
                pos_yes_1e6: self.fam[i].pos_yes_1e6,
                pos_no_1e6: self.fam[i].pos_no_1e6,
                p_raw_1e6: self.fam[i].p_raw_1e6,
                strike_1e6: self.fam[i].live.strike_1e6,
                mark_1e6: self.fam[i].mark_1e6,
                d_1e6: self.fam[i].d_1e6,
                den_1e9: self.fam[i].den_1e9,
            };
            i += 1;
        }
        n as u32
    }
}

impl Strategy for Bin15Strategy {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if self.configured == 0 {
            return Err(StrategyError::Config(
                "bin15: on_start before configure — a member with no families would run silent",
            ));
        }
        Ok(())
    }

    /// A tick on one of the binary legs updates that leg's touch and
    /// re-prices. Ticks on anything else — including the underlying's
    /// own BBO — are ignored: the MARK is the price source for the
    /// pricer, and the perp's book is not part of the law.
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        if self.configured == 0 {
            return;
        }
        let Some((idx, is_yes)) = self.family_of_sym(tick.sym) else {
            return;
        };
        let state = TouchState {
            touch: Touch {
                bid_1e6: tick.bid_px.raw(),
                ask_1e6: tick.ask_px.raw(),
                bid_qty_1e6: tick.bid_qty.raw(),
                ask_qty_1e6: tick.ask_qty.raw(),
            },
            ts_ns: tick.ts_ns,
            // VT4: a stale tick is not evidence. It still updates the
            // prices (they are the last thing the venue said) but the
            // flag keeps the arms off them.
            stale: u8::from(tick.is_stale()),
            _pad: [0; 7],
        };
        if is_yes {
            self.fam[idx].touch_yes = state;
        } else {
            self.fam[idx].touch_no = state;
        }
        self.reprice(ctx, idx, tick.ts_ns);
    }

    fn on_signal<C: Ctx>(&mut self, _signal: &core_types::Signal, _ctx: &mut C) {}

    fn on_fill<C: Ctx>(&mut self, fill: &core_types::Fill, _ctx: &mut C) {
        if self.configured == 0 {
            return;
        }
        self.book_fill(fill);
    }

    fn on_ai<C: Ctx>(&mut self, cmd: &core_types::AiCmd, _ctx: &mut C) {
        if self.configured == 0 {
            return;
        }
        if cmd.kind() != Some(AiCmdKind::SetBinarySpec) {
            return;
        }
        self.on_spec(cmd, cmd.ts_ns);
    }

    /// The two venue channels this member reads: the roll that says
    /// WHICH instrument a slot means, and the mark that prices it.
    fn on_venue_event<C: Ctx>(&mut self, event: &core_types::ChannelEvent, ctx: &mut C) {
        if self.configured == 0 {
            return;
        }
        match core_types::ChannelId::from_u8(event.channel) {
            Some(core_types::ChannelId::InstrumentRoll) => self.on_roll(event),
            Some(core_types::ChannelId::Mark) => {
                let Some(u) = self.underlying_of_sym(event.sym) else {
                    return;
                };
                self.on_mark(u, event.v0, event.ts_ns);
                // Every family on this underlying re-prices: one mark
                // moves every binary struck against it.
                let mut i = 0usize;
                while i < self.params.n_families {
                    if self.fam[i].underlying as usize == u {
                        self.reprice(ctx, i, event.ts_ns);
                    }
                    i += 1;
                }
            }
            _ => {}
        }
    }

    /// The 1 s cadence: deadlines, the day-cap epoch, the dormant
    /// level. NOT a re-price — a re-price with no new evidence returns
    /// the same number.
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, _ctx: &mut C) {
        if self.configured == 0 {
            return;
        }
        self.sweep_pendings(now_ns);
        self.roll_day(self.bar.anchor.wall_of(now_ns));
        self.refresh_dormant();
    }

    /// One second. A binary's fair value moves on marks and touches,
    /// not on the clock — the timer exists only to write off pendings,
    /// roll the day cap and recount the dormant level. `u64::MAX`
    /// before `configure`, so an unconfigured member costs the engine
    /// loop nothing.
    fn timer_period_ns(&self) -> u64 {
        if self.configured == 1 {
            1_000_000_000
        } else {
            u64::MAX
        }
    }

    /// The regime word is accepted and NOT consulted (the HORIZON
    /// law): the regime lane is measured on 4 h–8 h horizons and a
    /// 15-minute binary is not one of its cells.
    fn on_regime<C: Ctx>(&mut self, _gate: RegimeGate, _ctx: &mut C) {}

    /// The label the set stamps, CARRIED so the boot override seam
    /// (`regime.toml [labels.bin15]`) behaves like every other coded
    /// member's and `/state` reads the same field — but never
    /// consulted, per the HORIZON law above. Accepting the override
    /// and ignoring the word is the honest shape: refusing it would
    /// make `regime.toml` silently wrong about which members it
    /// configures.
    #[inline]
    fn regime_label(&self) -> core_types::RegimeLabelSet {
        self.regime_label
    }

    #[inline]
    fn set_regime_label(&mut self, set: core_types::RegimeLabelSet) -> bool {
        self.regime_label = set;
        true
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, ChannelEvent, ChannelId, Fill, TICK_FLAG_STALE};

    /// Monotonic boot instant.
    const MONO0: u64 = 5_000_000_000_000_000;
    /// Wall instant: 2026-09-12 06:00:00Z, on an exact minute.
    const WALL0: u64 = 1_789_192_800 * 1_000_000_000;

    /// The rolling ordinal base the universe reserves for HL families
    /// (`core_config::universe::HL_ROLLING_ORDINAL_BASE`).
    const ROLL_BASE: u32 = 4096;

    struct RecCtx {
        orders: Vec<Order>,
        full: bool,
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
            0
        }
    }
    fn ctx() -> RecCtx {
        RecCtx {
            orders: Vec::new(),
            full: false,
        }
    }

    fn yes_sym(f: u32) -> SymbolId {
        make_symbol_id(VenueId::Hyperliquid, ROLL_BASE + 2 * f)
    }
    fn no_sym(f: u32) -> SymbolId {
        make_symbol_id(VenueId::Hyperliquid, ROLL_BASE + 2 * f + 1)
    }
    fn btc_sym() -> SymbolId {
        make_symbol_id(VenueId::Hyperliquid, 3)
    }

    /// Φ tables that are a real CDF, not the identity: a piecewise
    /// linear ramp from 0.5 at `d = 0` to 1.0 at `d = 4.096`. Enough to
    /// make `p̂` monotone in moneyness, which is all these tests need —
    /// the exact table is pinned by the parity fixture instead.
    fn ramp_luts() -> Box<price::Bin15Luts> {
        let mut l = price::Bin15Luts::identity();
        let mut i = 0usize;
        while i < price::PHI_POINTS {
            let f = (i as i64 * 500_000) / (price::PHI_POINTS as i64 - 1);
            l.phi[i] = (500_000 + f) as u32;
            i += 1;
        }
        Box::new(l)
    }

    fn params_1(kind: u8) -> Bin15Params {
        let mut p = Bin15Params::default();
        p.sym_yes[0] = yes_sym(0);
        p.sym_no[0] = no_sym(0);
        p.family_underlying[0] = 0;
        p.family_kind[0] = kind;
        p.n_families = 1;
        p.underlying_sym[0] = btc_sym();
        p.n_underlyings = 1;
        // A daily family's floors are an hour, which no test window
        // reaches; shrink them so one code path covers both kinds.
        p.tau_min_take_ns = [60_000_000_000, 60_000_000_000];
        p.tau_min_quote_ns = [120_000_000_000, 120_000_000_000];
        p
    }

    fn member(kind: u8) -> Bin15Strategy {
        let mut m = Bin15Strategy::new();
        m.configure(
            params_1(kind),
            ramp_luts(),
            core_time::WallAnchor::new(MONO0, WALL0),
        )
        .expect("configure");
        m
    }

    /// Warm one underlying's forecast so `sig2_min` is non-zero: 1 441
    /// stamped returns fill the 1440-minute HAR window, and 60 pairs
    /// fit the line. Both tenors, because the underlying has one price.
    fn warm(m: &mut Bin15Strategy, u: usize) {
        let mut rets: Vec<(u64, i64)> = Vec::new();
        let mut i = 0u64;
        while i < 1_441 {
            // A deterministic alternating return, ~2 bps ×1e9.
            let r = if i % 3 == 0 { 20_000_000 } else { -12_000_000 };
            rets.push((WALL0 / 1_000_000 - (1_441 - i) * 60_000, r));
            i += 1;
        }
        m.seed_returns(u, &rets);
        let mut pairs: Vec<(u64, i64, i64)> = Vec::new();
        let mut k = 0i64;
        while k < 60 {
            pairs.push((
                WALL0 / 1_000_000 - (60 - k as u64) * 900_000,
                20_000_000_000 + k * 10_000_000,
                19_000_000_000 + k * 9_000_000,
            ));
            k += 1;
        }
        m.seed_pairs(u, FAMILY_OUT_15M, &pairs);
        m.seed_pairs(u, FAMILY_NATIVE_DAILY, &pairs);
    }

    fn roll_event(family: u8, outcome: u32, twap_s: u16, strike_1e6: i64, expiry_ns: u64, settled: bool) -> ChannelEvent {
        let seq = u64::from(outcome)
            | (u64::from(twap_s) << 32)
            | (u64::from(family) << 48)
            | (u64::from(settled) << 56);
        ChannelEvent::new(
            MONO0,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            yes_sym(u32::from(family)),
            seq,
            0,
            strike_1e6,
            expiry_ns as i64,
        )
    }

    fn mark_event(mark_1e6: i64, at: NsTs) -> ChannelEvent {
        ChannelEvent::new(
            at,
            VenueId::Hyperliquid,
            ChannelId::Mark,
            btc_sym(),
            0,
            0,
            mark_1e6,
            0,
        )
    }

    fn tick(sym: SymbolId, bid: i64, ask: i64, qty: i64, at: NsTs, stale: bool) -> Tick {
        let mut t = Tick::new(
            at,
            VenueId::Hyperliquid,
            sym,
            0,
            Price::from_raw(bid),
            Qty::from_raw(qty),
            Price::from_raw(ask),
            Qty::from_raw(qty),
        );
        if stale {
            t.flags |= TICK_FLAG_STALE;
        }
        t
    }

    /// Wall instant `secs` after the anchor, as a monotonic stamp.
    const fn at(secs: u64) -> NsTs {
        MONO0 + secs * 1_000_000_000
    }

    /// An expiry `secs` after the anchor, in WALL ns.
    const fn expiry(secs: u64) -> u64 {
        WALL0 + secs * 1_000_000_000
    }

    /// Drive one family to a priced state: warm, bind, mark, book.
    fn live_family(m: &mut Bin15Strategy, c: &mut RecCtx, yes_bid: i64, yes_ask: i64) {
        warm(m, 0);
        m.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(600), false), c);
        // Two marks a minute apart, so the minute rolls and
        // `refresh_sigma` runs.
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), c);
        m.on_tick(&tick(no_sym(0), 400_000, 600_000, 1_000_000_000, at(62), false), c);
        m.on_tick(&tick(yes_sym(0), yes_bid, yes_ask, 1_000_000_000, at(62), false), c);
    }

    #[test]
    fn on_start_refuses_an_unconfigured_member() {
        let mut m = Bin15Strategy::new();
        let mut c = ctx();
        assert!(m.on_start(&mut c).is_err(), "a member with no families must refuse");
        assert_eq!(m.timer_period_ns(), u64::MAX, "and cost the loop nothing");
        assert!(member(FAMILY_OUT_15M).on_start(&mut c).is_ok());
        assert_eq!(member(FAMILY_OUT_15M).timer_period_ns(), 1_000_000_000);
    }

    #[test]
    fn configure_refuses_every_shape_it_cannot_trade() {
        let luts = || ramp_luts();
        let anchor = core_time::WallAnchor::new(MONO0, WALL0);
        let mut bad = params_1(FAMILY_OUT_15M);
        bad.n_families = 0;
        assert!(Bin15Strategy::new().configure(bad, luts(), anchor).is_err());
        let mut bad = params_1(FAMILY_OUT_15M);
        bad.sym_no[0] = SYMBOL_ID_NONE;
        assert!(Bin15Strategy::new().configure(bad, luts(), anchor).is_err());
        let mut bad = params_1(FAMILY_OUT_15M);
        bad.family_underlying[0] = 3;
        assert!(Bin15Strategy::new().configure(bad, luts(), anchor).is_err());
        let mut bad = params_1(FAMILY_OUT_15M);
        bad.scale_1e9 = 0;
        assert!(Bin15Strategy::new().configure(bad, luts(), anchor).is_err());
        let mut bad = params_1(FAMILY_OUT_15M);
        bad.e_take_1e6 = GRID_TICK_1E6 - 1;
        assert!(Bin15Strategy::new().configure(bad, luts(), anchor).is_err());
        let mut bad = params_1(FAMILY_OUT_15M);
        bad.tau_min_take_ns[0] = TAU_15M_NS;
        assert!(Bin15Strategy::new().configure(bad, luts(), anchor).is_err());
        let mut bad = params_1(FAMILY_OUT_15M);
        bad.cap_day_usd_1e6 = 0;
        assert!(Bin15Strategy::new().configure(bad, luts(), anchor).is_err());
    }

    #[test]
    fn a_roll_binds_the_instance_and_arms_the_engine() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        assert_eq!(m.counters().families_dormant, 1, "nothing bound yet");
        m.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(900), false), &mut c);
        let f = m.family(0).expect("family");
        assert_eq!(f.live.outcome, 2649);
        assert_eq!(f.live.strike_1e6, 79_000_000_000);
        assert_eq!(f.live.expiry_ns, expiry(900));
        assert_eq!(f.live.twap_ns, 60_000_000_000, "60 s TWAP off the packed seq");
        assert_eq!(m.counters().rolls, 1);
        assert_eq!(m.counters().families_dormant, 0);
        assert!(m.marks[0].vol[0].is_armed(), "the 15 m engine holds");
        assert!(!m.marks[0].vol[1].is_armed(), "the 8 h engine does NOT");

        // The settled row clears the slot and settles the hold.
        m.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(900), true), &mut c);
        assert_eq!(m.family(0).expect("family").live.outcome, 0);
        assert_eq!(m.counters().rolls_settled, 1);
        assert_eq!(m.counters().families_dormant, 1);
        assert!(!m.marks[0].vol[0].is_armed());
    }

    #[test]
    fn a_settled_row_for_an_outcome_nobody_holds_clears_nothing() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        m.on_venue_event(&roll_event(0, 2650, 60, 79_000_000_000, expiry(900), false), &mut c);
        // A late duplicate for the PREVIOUS instance must not clear the
        // successor that has already bound.
        m.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(0), true), &mut c);
        assert_eq!(m.family(0).expect("f").live.outcome, 2650, "the successor survives");
        assert_eq!(m.counters().rolls_settled, 0);
        assert_eq!(m.counters().spec_refused, 1, "and the stray frame is counted");
    }

    #[test]
    fn dormant_family_emits_nothing() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), &mut c);
        m.on_tick(&tick(yes_sym(0), 100_000, 200_000, 1_000_000_000, at(62), false), &mut c);
        m.on_tick(&tick(no_sym(0), 700_000, 800_000, 1_000_000_000, at(62), false), &mut c);
        assert!(c.orders.is_empty(), "no instance, no intent");
        assert_eq!(m.counters().reprices, 0);
        assert_eq!(m.orders_emitted(), 0);
    }

    #[test]
    fn a_take_fires_only_when_the_touch_is_wrong_by_e() {
        // At the money with a ramp Φ, p̂ ≈ 0.5. An ask of 0.40 is 10 c
        // cheap — well past the 3 c edge.
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        live_family(&mut m, &mut c, 390_000, 400_000);
        assert!(m.counters().reprices > 0, "the pricer ran");
        assert_eq!(m.counters().takes_submitted, 1, "one IoC, not two");
        let o = c.orders.iter().find(|o| o.kind == ORDER_KIND_IOC).expect("an IoC");
        assert_eq!(o.sym, yes_sym(0), "the cheap side is Yes");
        assert_eq!(o.side, Side::Bid, "and the member BUYS it");
        assert_eq!(o.px.raw(), 400_000, "at the touch, not through it");
        assert_eq!(o.venue, VenueId::Hyperliquid as u8);

        // A touch inside the edge fires nothing.
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        live_family(&mut m, &mut c, 480_000, 490_000);
        assert_eq!(m.counters().takes_submitted, 0, "10 c away is not 1 c away");
        assert!(c.orders.iter().all(|o| o.kind != ORDER_KIND_IOC));
    }

    #[test]
    fn a_stale_touch_is_never_acted_on() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        m.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(600), false), &mut c);
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), &mut c);
        m.on_tick(&tick(no_sym(0), 400_000, 600_000, 1_000_000_000, at(62), false), &mut c);
        m.on_tick(&tick(yes_sym(0), 390_000, 400_000, 1_000_000_000, at(62), true), &mut c);
        assert_eq!(m.counters().takes_submitted, 0, "VT4: stale is not evidence");
        assert!(m.counters().skipped_book > 0);
        // The prices ARE recorded — they are the last thing the venue
        // said — and the next FRESH tick acts on them.
        assert_eq!(m.family(0).expect("f").touch_yes.touch.ask_1e6, 400_000);
        m.on_tick(&tick(yes_sym(0), 390_000, 400_000, 1_000_000_000, at(63), false), &mut c);
        assert_eq!(m.counters().takes_submitted, 1);
    }

    #[test]
    fn no_take_inside_tail_refuse() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        // Expiry 5 s out, `tail_refuse_ns` is 10 s.
        m.on_venue_event(&roll_event(0, 2649, 0, 79_000_000_000, expiry(5), false), &mut c);
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), &mut c);
        m.on_tick(&tick(no_sym(0), 400_000, 600_000, 1_000_000_000, at(62), false), &mut c);
        m.on_tick(&tick(yes_sym(0), 100_000, 200_000, 1_000_000_000, at(62), false), &mut c);
        assert_eq!(m.counters().takes_submitted, 0);
        assert_eq!(m.counters().reprices, 0, "the pricer never even ran");
        assert!(m.counters().skipped_tail > 0);
    }

    #[test]
    fn no_quote_inside_tau_min() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        // The mark events must straddle a MINUTE boundary or
        // `refresh_sigma` never runs and the pricer holds on Stale
        // instead — which would make this test pass for the wrong
        // reason.
        m.on_venue_event(&roll_event(0, 2649, 0, 79_000_000_000, expiry(160), false), &mut c);
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), &mut c);
        // 98 s to expiry: past `tau_min_take` (60 s), inside
        // `tau_min_quote` (120 s).
        m.on_tick(&tick(no_sym(0), 400_000, 600_000, 1_000_000_000, at(62), false), &mut c);
        m.on_tick(&tick(yes_sym(0), 490_000, 500_000, 1_000_000_000, at(62), false), &mut c);
        assert!(m.counters().reprices > 0, "the pricer DID run");
        assert_eq!(m.counters().quotes_submitted, 0, "no maker this close in");
        assert!(m.counters().skipped_tau > 0);
        assert!(c.orders.iter().all(|o| o.kind != ORDER_KIND_MAKER));
    }

    #[test]
    fn arm_b_quotes_the_bid_and_asks_only_against_inventory() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        live_family(&mut m, &mut c, 490_000, 510_000);
        let makers: Vec<&Order> = c.orders.iter().filter(|o| o.kind == ORDER_KIND_MAKER).collect();
        assert_eq!(makers.len(), 1, "the bid alone: no Yes inventory to offer");
        assert_eq!(makers[0].side, Side::Bid);
        assert_eq!(makers[0].sym, yes_sym(0), "Arm B quotes the Yes book");
        assert_eq!(makers[0].px.raw() % GRID_TICK_1E6, 0, "on the 1e-4 grid");
        assert!(makers[0].px.raw() <= 490_000, "never crossing the touch");
        assert!(m.counters().skipped_inventory > 0, "and the ask side says why");
    }

    #[test]
    fn positions_move_only_on_fills() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        live_family(&mut m, &mut c, 390_000, 400_000);
        assert_eq!(m.family(0).expect("f").pos_yes_1e6, 0, "a submit is an intent");
        let oid = m.family(0).expect("f").pend_take.oid;
        assert_ne!(oid, 0);
        m.on_fill(
            &Fill::new(at(63), yes_sym(0), Side::Bid, Price::from_raw(400_000), Qty::from_raw(25_000_000), oid),
            &mut c,
        );
        assert_eq!(m.family(0).expect("f").pos_yes_1e6, 25_000_000, "NOW it moves");
        assert_eq!(m.counters().fills, 1);
        assert_eq!(m.counters().takes_filled, 1);
        assert_eq!(m.counters().unknown_fills, 0);
        assert_eq!(m.family(0).expect("f").pend_take.oid, 0, "an IoC is judged once");

        // A fill for an order this member never sent changes nothing.
        m.on_fill(
            &Fill::new(at(64), yes_sym(0), Side::Bid, Price::from_raw(400_000), Qty::from_raw(1_000_000), 0xDEAD),
            &mut c,
        );
        assert_eq!(m.family(0).expect("f").pos_yes_1e6, 25_000_000);
        assert_eq!(m.counters().unknown_fills, 1);
    }

    #[test]
    fn a_sell_can_never_take_a_side_below_zero() {
        // The venue has no short, so a sell fill larger than the
        // position must floor at zero rather than wrap negative.
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        live_family(&mut m, &mut c, 390_000, 400_000);
        let oid = m.family(0).expect("f").pend_take.oid;
        m.on_fill(
            &Fill::new(at(63), yes_sym(0), Side::Bid, Price::from_raw(400_000), Qty::from_raw(5_000_000), oid),
            &mut c,
        );
        assert_eq!(m.family(0).expect("f").pos_yes_1e6, 5_000_000);
        m.apply_position(0, true, false, 9_000_000);
        assert_eq!(m.family(0).expect("f").pos_yes_1e6, 0, "floored, not negative");
    }

    #[test]
    fn an_unfilled_take_is_counted_and_cleared() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        live_family(&mut m, &mut c, 390_000, 400_000);
        assert!(m.family(0).expect("f").pend_take.live());
        // Before the deadline: still pending.
        m.on_timer(at(62), &mut c);
        assert!(m.family(0).expect("f").pend_take.live());
        assert_eq!(m.counters().takes_unfilled, 0);
        // After: written off.
        m.on_timer(at(70), &mut c);
        assert!(!m.family(0).expect("f").pend_take.live());
        assert_eq!(m.counters().takes_unfilled, 1);
        assert_eq!(m.counters().quotes_expired, 1, "the resting bid expired too");
    }

    #[test]
    fn null_arm_alternates_by_outcome_parity() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        m.on_venue_event(&roll_event(0, 2648, 60, 79_000_000_000, expiry(900), false), &mut c);
        assert_eq!(m.family(0).expect("f").arm, ARM_MODEL, "even outcome ⇒ model");
        m.on_venue_event(&roll_event(0, 2648, 60, 79_000_000_000, expiry(900), true), &mut c);
        m.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(900), false), &mut c);
        assert_eq!(m.family(0).expect("f").arm, ARM_NULL, "odd outcome ⇒ null");

        // With `null_arm` off, every instance is the model arm.
        let mut p = params_1(FAMILY_OUT_15M);
        p.null_arm = 0;
        let mut m2 = Bin15Strategy::new();
        m2.configure(p, ramp_luts(), core_time::WallAnchor::new(MONO0, WALL0)).expect("cfg");
        warm(&mut m2, 0);
        m2.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(900), false), &mut c);
        assert_eq!(m2.family(0).expect("f").arm, ARM_MODEL);
    }

    #[test]
    fn the_oid_carries_arm_family_side_and_outcome() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        live_family(&mut m, &mut c, 390_000, 400_000);
        let oid = m.family(0).expect("f").pend_take.oid;
        assert_eq!(oid & OID_OUTCOME_MASK, 2649, "outcome in bits 0..32");
        assert_eq!((oid >> OID_FAMILY_SHIFT) & 0xFFFF, 0, "family in 32..48");
        assert_eq!((oid >> OID_SIDE_SHIFT) & 1, 0, "Yes leg");
        assert_eq!(oid >> OID_ARM_SHIFT, 1, "outcome 2649 is odd ⇒ null arm");
        assert_ne!(oid, 0, "and an oid is never zero, which is what marks a slot free");
    }

    #[test]
    fn a_setbinaryspec_override_rebinds_and_counts() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        let bind = core_types::AiCmd::new(
            at(1),
            1,
            yes_sym(0),
            79_500_000_000,
            expiry(900) as i64,
            2700,
            AiCmdKind::SetBinarySpec,
            VenueId::Ai,
            core_types::STRATEGY_SLOT_BIN15,
            core_types::AI_SIDE_NONE,
            60,
            0,
        );
        assert_eq!(bind.validate_shape(), Ok(()), "the wire gate passes it");
        m.on_ai(&bind, &mut c);
        assert_eq!(m.counters().spec_overrides, 1);
        assert_eq!(m.family(0).expect("f").live.outcome, 2700);
        assert_eq!(m.family(0).expect("f").live.strike_1e6, 79_500_000_000);

        // An expiry in the PAST is refused: the clock check is the
        // member's, because the wire gate runs offline too.
        let mut stale = bind;
        stale.qty = (WALL0 - 1) as i64;
        stale.ttl_ns = 2701;
        m.on_ai(&stale, &mut c);
        assert_eq!(m.counters().spec_refused, 1);
        assert_eq!(m.family(0).expect("f").live.outcome, 2700, "unchanged");

        // A sym that is not a configured Yes slot is refused.
        let mut wrong = bind;
        wrong.sym = no_sym(0);
        m.on_ai(&wrong, &mut c);
        assert_eq!(m.counters().spec_refused, 2);

        // And a CLEAR settles the slot.
        let mut clear = bind;
        clear.px = 0;
        clear.qty = 0;
        clear.flags = core_types::AI_CMD_FLAG_CLEAR_SPEC;
        assert_eq!(clear.validate_shape(), Ok(()));
        m.on_ai(&clear, &mut c);
        assert_eq!(m.family(0).expect("f").live.outcome, 0);
        assert_eq!(m.counters().spec_overrides, 2);
    }

    #[test]
    fn caps_hold_per_instance_and_per_day() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        // $1 k per instance at 0.40 is 2 500 contracts; the clip is
        // 500, so five takes exhaust the instance cap.
        warm(&mut m, 0);
        m.on_venue_event(&roll_event(0, 2648, 60, 79_000_000_000, expiry(600), false), &mut c);
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), &mut c);
        m.on_tick(&tick(no_sym(0), 400_000, 600_000, 1_000_000_000, at(62), false), &mut c);
        let mut t = 62u64;
        let mut submitted_before = 0u64;
        let mut i = 0usize;
        while i < 12 {
            // Each take is written off by its deadline, so the next
            // tick can submit again — the caps, not the pendings, are
            // what eventually stops it.
            m.on_timer(at(t + 2), &mut c);
            m.on_tick(&tick(yes_sym(0), 390_000, 400_000, 500_000_000, at(t + 3), false), &mut c);
            submitted_before = m.counters().takes_submitted;
            t += 3;
            i += 1;
        }
        assert!(submitted_before >= 2, "it did trade");
        // THE INVARIANT: the booked notional never exceeds the cap, no
        // matter how many ticks arrive.
        let f = m.family(0).expect("f");
        assert!(
            f.notional_instance_1e6 <= m.params().cap_instance_usd_1e6,
            "instance notional {} over cap",
            f.notional_instance_1e6
        );
        // AND the interaction worth knowing: the last sliver of cap
        // room does not produce a tiny order, it produces NO order,
        // because the residual buys fewer contracts than the venue's
        // $10 minimum notional. So the proximate refusal is the GRID,
        // not the cap — the same interaction O3 documented in the
        // harness. An operator sees `skipped_grid` climbing with the
        // notional gauge pinned at the cap, which is the truth.
        assert!(
            m.counters().skipped_grid > 0,
            "the residual cap room must be refused, not rounded up"
        );
        assert!(
            m.params().cap_instance_usd_1e6 - f.notional_instance_1e6
                < GRID_MIN_NOTIONAL_1E6 as i64,
            "and what is left must be under the venue minimum: {} left",
            m.params().cap_instance_usd_1e6 - f.notional_instance_1e6
        );
        // The day cap survives a roll; the instance cap does not.
        let day_before = m.day_notional_1e6;
        assert!(day_before > 0);
        m.on_venue_event(&roll_event(0, 2648, 60, 79_000_000_000, expiry(600), true), &mut c);
        m.on_venue_event(&roll_event(0, 2650, 60, 79_000_000_000, expiry(1200), false), &mut c);
        assert_eq!(m.family(0).expect("f").notional_instance_1e6, 0, "per instance");
        assert_eq!(m.day_notional_1e6, day_before, "per day, across the roll");
        // And it rolls at 00:00Z.
        m.on_timer(at(86_400 * 2), &mut c);
        assert_eq!(m.day_notional_1e6, 0);
    }

    #[test]
    fn dailies_use_the_8h_engine() {
        let mut m = member(FAMILY_NATIVE_DAILY);
        let mut c = ctx();
        warm(&mut m, 0);
        m.on_venue_event(&roll_event(0, 2649, 0, 79_000_000_000, expiry(43_200), false), &mut c);
        // A daily instance arms NEITHER engine on the roll: the 8 h
        // engine's holds belong to its own 8 h grid, and arming it on a
        // 24 h boundary would pair an 8 h fit with a 24 h window.
        assert!(!m.marks[0].vol[0].is_armed());
        assert!(!m.marks[0].vol[1].is_armed());
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), &mut c);
        // Both σ slots are published, and the 8 h one is the LARGER
        // per-minute variance only if the tenors disagree — what
        // matters is that the DAILY family priced off index 1.
        assert!(m.marks[0].sig2_min_1e18[1] > 0, "the 8 h variance exists");
        m.on_tick(&tick(no_sym(0), 400_000, 600_000, 1_000_000_000, at(62), false), &mut c);
        m.on_tick(&tick(yes_sym(0), 390_000, 400_000, 1_000_000_000, at(62), false), &mut c);
        assert!(m.counters().reprices > 0, "and the daily priced");
        assert_ne!(
            m.marks[0].sig2_min_1e18[0], m.marks[0].sig2_min_1e18[1],
            "the two tenors are different numbers, or the split is decoration"
        );
    }

    /// O4b: the unit bridge, pinned from BOTH ends.
    ///
    /// First that `sig2_min_1e18` is exactly what `core-vol`'s own σ̂
    /// implies once the bps→fraction conversion is applied — an
    /// identity, so a future edit to either side has to move both.
    /// Then that a REALISTIC volatility produces a realistic `d`: this
    /// is the assertion that was missing while σ̂ was 1e4 too large and
    /// every binary priced at a coin flip.
    #[test]
    fn the_per_minute_variance_is_in_the_pricers_units_not_core_vols() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), &mut c);
        // The identity, both tenors.
        let mut k = 0usize;
        while k < 2 {
            let tau = tau_of_kind(k as u8);
            let sig_bps_1e9 = m.marks[0].vol[k].sigma_hat_1e9(tau).expect("sigma");
            let sig = i128::from(sig_bps_1e9);
            let got = m.marks[0].sig2_min_1e18[k];
            // (a) The exact identity, in the member's own order of
            // operations (`scale_1e9` is 1e9 here, so it drops out).
            assert_eq!(
                got,
                (sig * sig) / (BPS2_TO_FRAC2_1E8 * tau_minutes_of_kind(k as u8)),
                "tenor {k}: sig2 is not σ̂ {sig_bps_1e9} through the bridge"
            );
            // (b) And that the constant IS the bps→fraction bridge and
            // not a tuning knob: reading σ̂ as a fraction ×1e9 FIRST
            // and then squaring gives the same number, to within the
            // digits that earlier division floors away.
            let frac_1e9 = sig / 10_000;
            let two_step = (frac_1e9 * frac_1e9) / tau_minutes_of_kind(k as u8);
            assert!(
                (got - two_step).abs() * 1_000 <= two_step,
                "tenor {k}: {got} and {two_step} are not the same quantity"
            );
            k += 1;
        }

        // The magnitude. σ over a full quarter-hour of 20 bps is an
        // ordinary BTC 15 m: σ_min = 20/√15 bps = 5.164e-4 as a
        // FRACTION, so the per-minute variance ×1e18 is 2.667e11.
        m.marks[0].sig2_min_1e18[0] = 266_700_000_000;
        // The instance expires 600 s after the anchor and the mark
        // lands at 62 s, so τ = 538 s and σ_τ = 20·√(538/900) = 15.47
        // bps = 1.5464e-3. A mark 25 bps above the strike is then
        // x/σ_τ = 2.4969e-3 / 1.5464e-3 = 1.615 standard deviations in
        // the money. The ramp LUT is linear from 0.5 at d = 0 to 1.0 at
        // d = 4.096, so it reads 0.5 + 0.5·1.615/4.096 = 0.6971. (A
        // real CDF would say 0.947 — the ramp is not a CDF, which is
        // why the parity fixture pins the shipped table and this test
        // pins only the SCALE.)
        //
        // What matters is the order of magnitude: with σ̂ 1e4 too large
        // this same input printed 0.50002, and every counter in the
        // member still moved.
        m.on_venue_event(&roll_event(0, 2650, 0, 79_000_000_000, expiry(600), false), &mut c);
        m.on_venue_event(&mark_event(79_197_500_000, at(62)), &mut c);
        let fam = m.family(0).expect("f");
        assert!(fam.p_ts_ns != 0, "the instance priced");
        // With a LINEAR Φ, `p_raw` pins `d` exactly. (BIN15 O6 also
        // publishes `d` and its denominator outright — see the test
        // below — but this assertion stays on `p_raw` because that is
        // the number the SCALE bug moved.)
        assert!(
            (690_000..=705_000).contains(&fam.p_raw_1e6),
            "p_raw {} — a 25 bps lead at 1.6σ must not price like a coin flip",
            fam.p_raw_1e6
        );
        assert_eq!(
            fam.p_hat_1e6, fam.p_raw_1e6,
            "the identity recalibration leaves it alone"
        );
    }

    /// BIN15 O6: a repriced family publishes every input behind its
    /// fair value, and drops them when the instance settles.
    ///
    /// The sibling test above can only infer `d` from `p_raw`, and
    /// says so. These gauges are that gap closed. The reason they
    /// exist: on 2026-09-12 a live `p̂` sat at exactly 1e6 for minutes
    /// and nothing on the box could say whether σ̂ was too small or the
    /// move was real — answering it took an offline replay.
    #[test]
    fn a_repriced_family_publishes_every_input_and_drops_them_at_settle() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        // Same σ̂ and geometry as the scale test above.
        m.marks[0].sig2_min_1e18[0] = 266_700_000_000;
        m.on_venue_event(
            &roll_event(0, 2650, 0, 79_000_000_000, expiry(600), false),
            &mut c,
        );
        m.on_venue_event(&mark_event(79_197_500_000, at(62)), &mut c);

        let mut view = [strategy_core::Bin15FamilyView::default(); 8];
        let n = m.bin15_families_view(&mut view) as usize;
        assert!(n >= 1, "the configured family is in the view");
        let v = view[0];
        let fam = *m.family(0).expect("f");
        assert_eq!(v.live_outcome, 2650);
        assert_eq!(v.strike_1e6, 79_000_000_000, "the instance's threshold");
        assert_eq!(v.mark_1e6, 79_197_500_000, "the mark the reprice used");
        // τ = 600 s expiry − the 62 s mark, plus a third of a zero TWAP.
        assert_eq!(v.tau_s, 538);
        assert!(v.den_1e9 > 0, "σ√τ must be readable, not inferred");
        assert!(v.d_1e6 > 0, "a mark above the strike is a positive d");
        assert_eq!(v.p_raw_1e6, fam.p_raw_1e6);
        assert_eq!(v.p_hat_1e6, fam.p_hat_1e6);
        // The published pair reproduces the published `d`: that is the
        // whole point — a reader can redo the member's division.
        let x_1e9 = price::log_moneyness_1e9(v.mark_1e6, v.strike_1e6).expect("x");
        let redone = (i128::from(x_1e9) * 1_000_000) / i128::from(v.den_1e9);
        assert_eq!(redone as i64, v.d_1e6, "d = x / (σ√τ), both published");

        // Settle: the diagnostics describe the instance, so they go
        // with it. A denominator left behind would read as a live one.
        m.on_venue_event(&roll_event(0, 2650, 0, 0, 0, true), &mut c);
        let n = m.bin15_families_view(&mut view) as usize;
        assert!(n >= 1);
        assert_eq!(view[0].live_outcome, 0, "dormant");
        assert_eq!(view[0].den_1e9, 0);
        assert_eq!(view[0].mark_1e6, 0);
        assert_eq!(view[0].strike_1e6, 0);
        assert_eq!(view[0].d_1e6, 0);
        assert_eq!(view[0].tau_s, 0);
    }

    /// BIN15 O9: the coverage entry takes EVERY 15 m instance at the
    /// fixed notional, on the side `p̂` prefers, with no edge required
    /// — and exactly once per instance.
    ///
    /// Why it exists: 22 of the 33 instances in the 2026-09-12 capture
    /// drew no order at all, so two thirds of the calibration ledger
    /// was never written.
    #[test]
    fn the_coverage_entry_takes_every_instance_once_at_the_fixed_size() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        m.params.entry_usd_1e6 = 50_000_000; // $50
        m.marks[0].sig2_min_1e18[0] = 266_700_000_000;
        // A mark well ABOVE the strike ⇒ p̂ > 0.5 ⇒ the YES side.
        m.on_venue_event(
            &roll_event(0, 2650, 0, 79_000_000_000, expiry(600), false),
            &mut c,
        );
        m.on_venue_event(&mark_event(79_197_500_000, at(62)), &mut c);
        // A two-sided book with NO edge on either leg: p̂ ≈ 0.697, so
        // a YES ask of 0.69 and a NO ask of 0.30 both sit inside
        // `e_take` (0.03) of fair. The edge arm cannot fire here — any
        // order is the coverage entry's.
        m.on_tick(&tick(yes_sym(0), 680_000, 690_000, 1_000_000_000, at(63), false), &mut c);
        m.on_tick(&tick(no_sym(0), 290_000, 300_000, 1_000_000_000, at(63), false), &mut c);
        m.on_venue_event(&mark_event(79_197_500_000, at(64)), &mut c);

        // The MAKER arm also quotes here; the coverage entry is the
        // taker, so count those.
        let takes = |c: &RecCtx| -> Vec<core_types::Order> {
            c.orders
                .iter()
                .filter(|o| o.kind == ORDER_KIND_IOC)
                .copied()
                .collect()
        };
        let t = takes(&c);
        assert_eq!(t.len(), 1, "one coverage entry, not one per reprice");
        let o = t[0];
        assert_eq!(o.sym, yes_sym(0), "p̂ > 0.5 takes the YES leg");
        assert_eq!(o.side, Side::Bid);
        assert_eq!(o.px.raw(), 690_000, "lifts the ask");
        // $50 / 0.69 = 72.46 contracts, floored to the venue's lot.
        assert_eq!(o.qty.raw(), 72_000_000, "$50 at 0.69 is 72 contracts");

        // A second reprice must NOT enter again: the instance is taken.
        m.on_venue_event(&mark_event(79_198_000_000, at(70)), &mut c);
        assert_eq!(takes(&c).len(), 1, "once per instance");

        // The NEXT instance is entered afresh.
        m.on_venue_event(&roll_event(0, 2650, 0, 0, 0, true), &mut c);
        m.on_venue_event(
            &roll_event(0, 2651, 0, 79_000_000_000, expiry(1500), false),
            &mut c,
        );
        // The settle feeds the realised vol back into the forecast,
        // which drops the hand-set σ̂ this test stands on — put it back,
        // exactly as at the top, or the successor never prices.
        m.marks[0].sig2_min_1e18[0] = 266_700_000_000;
        m.on_venue_event(&mark_event(79_197_500_000, at(962)), &mut c);
        m.on_tick(&tick(yes_sym(0), 680_000, 690_000, 1_000_000_000, at(963), false), &mut c);
        m.on_tick(&tick(no_sym(0), 290_000, 300_000, 1_000_000_000, at(963), false), &mut c);
        // Immediately before the reprice: the settle fed realised vol
        // back into the forecast and a mark can refresh σ̂ from it, so
        // the hand-set value has to be the last word.
        m.marks[0].sig2_min_1e18[0] = 266_700_000_000;
        m.on_venue_event(&mark_event(79_197_500_000, at(964)), &mut c);
        assert_eq!(takes(&c).len(), 2, "the successor is its own instance");
    }

    /// BIN15 O9: `entry_usd_1e6 = 0` is the pre-2026-09-13 law bit for
    /// bit — no coverage entry, the edge arm alone decides.
    #[test]
    fn a_zero_entry_notional_is_the_edge_law_unchanged() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        assert_eq!(m.params.entry_usd_1e6, 0, "off by default");
        m.marks[0].sig2_min_1e18[0] = 266_700_000_000;
        m.on_venue_event(
            &roll_event(0, 2650, 0, 79_000_000_000, expiry(600), false),
            &mut c,
        );
        m.on_venue_event(&mark_event(79_197_500_000, at(62)), &mut c);
        // The same no-edge book the coverage test uses.
        m.on_tick(&tick(yes_sym(0), 680_000, 690_000, 1_000_000_000, at(63), false), &mut c);
        m.on_tick(&tick(no_sym(0), 290_000, 300_000, 1_000_000_000, at(63), false), &mut c);
        m.on_venue_event(&mark_event(79_197_500_000, at(64)), &mut c);
        assert!(
            c.orders.iter().all(|o| o.kind != ORDER_KIND_IOC),
            "no taker order without edge when the coverage entry is off"
        );
    }

    #[test]
    fn a_full_order_ring_is_counted_and_books_nothing() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        c.full = true;
        live_family(&mut m, &mut c, 390_000, 400_000);
        // TWO, not three: the take and the BID quote reach `submit`
        // and are refused by the ring, while the ASK quote never gets
        // that far — with no Yes inventory the short rule holds it
        // before an order exists. A count of three would mean the
        // member had tried to sell what it does not own.
        assert_eq!(m.orders_dropped(), 2, "the take and the bid quote");
        assert!(m.counters().skipped_inventory > 0, "and the ask said why");
        assert_eq!(m.orders_emitted(), 0);
        assert_eq!(m.counters().takes_submitted, 0, "a dropped order is not a submit");
        assert!(!m.family(0).expect("f").pend_take.live(), "and books no pending");
        assert_eq!(m.family(0).expect("f").notional_instance_1e6, 0, "nor any notional");
    }

    #[test]
    fn the_regime_word_is_carried_and_never_consulted() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        let labels = core_types::RegimeLabelSet::default();
        m.set_regime_label(labels);
        assert_eq!(m.regime_label(), labels);
        // A CLOSED gate changes nothing: the HORIZON law.
        live_family(&mut m, &mut c, 390_000, 400_000);
        let before = m.counters().takes_submitted;
        m.on_regime(
            RegimeGate::new(
                [core_types::RegimeWord(0); 4],
                false,
                core_types::REGIME_OFF_HARD,
            ),
            &mut c,
        );
        m.on_timer(at(70), &mut c);
        m.on_tick(&tick(yes_sym(0), 390_000, 400_000, 1_000_000_000, at(71), false), &mut c);
        assert!(
            m.counters().takes_submitted > before,
            "a 15-minute binary is not one of the regime lane's cells"
        );
    }
}
