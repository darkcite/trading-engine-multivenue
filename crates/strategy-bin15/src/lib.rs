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
//! the BTC 15-minute family rolls 96 times a day.
//!
//! **BIN15 H90 (2026-10-16, ruling O-GL-2): the member trades ONE law,
//! the late-window entry** (`late` module, vault docs 32–34): inside the
//! settlement minute it prices the partially observed 60 s mark TWAP
//! from a short horizon and the perp's book (P1b), and takes one IoC per
//! instance in `[late_tau_min, late_tau_max)` when the favoured side's
//! ask sits at least the late edge under its probability. It is OFF
//! unless the artifact switches it on.
//!
//! The S-law pricer stays as the LOGGED counterfactual (the late first
//! fires' `ctl_*` columns) and the calibration ledger; it never trades.
//! Per re-price it is: the log-moneyness of the underlying mark against
//! the strike to second order; divided by `σ_min·√τ` built from the
//! once-a-minute `sigma_hat_1e9`; looked up in a Φ table on a 1e-3
//! grid; then recalibrated through one of three τ-phase tables. One
//! `i128` mul-div, one integer square root, four table reads. No float
//! anywhere, no transcendental on the decision path.
//!
//! The coverage entry, Arm A's takes and closes, Arm B's maker and the
//! null arm were REMOVED at H90 G3 (vault doc 38 records their law and
//! the commit to recover them from).
//!
//! ## What this lane does NOT do
//!
//! * **It does not infer a position from a submit.** Positions come
//!   from fills; an unfilled take is written off at its deadline and
//!   gives its room back.
//! * **It never sells.** A HIP-4 position cannot go negative: the two
//!   sides of a binary are two separate instruments, and every order
//!   this member emits is a BUY of the favoured side, held to
//!   settlement.
//! * **The S-law pricer stops in the last `tail_refuse_ns`.** Its fair
//!   value there is a step function of the next tick of the underlying.
//!   The late law's floor is its own (`late_tau_min`); the venue clears
//!   the book at expiry.
//! * **It does not consult the regime word.** The HORIZON law: the
//!   regime lane is measured on 4 h–8 h horizons and a 15-minute binary
//!   is not one of its cells. The gate is accepted and ignored, and
//!   that is deliberate rather than an omission.
//! * **It does not build a signer, a cancel or a dispatch path.** Those
//!   belong to the execution lane (`exec-hyperliquid`).
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

use core_fill::{Touch, ORDER_KIND_IOC};
use core_time::{BarClock, NsTs};
use core_types::{
    AiCmdKind, Order, Price, Qty, Side, SymbolId, Tick, VenueId, SYMBOL_ID_NONE,
};
use strategy_core::{
    Bin15Counters, Ctx, RegimeGate, Strategy, StrategyCounters, StrategyError, SubmitErr,
};

pub mod late;
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

/// The smallest edge a law may ask for ×1e6 (1e-4 of a dollar of payout).
/// NOT the venue's price law: that is [`PX_SIG_FIGS`] (H90 G2).
pub const GRID_TICK_1E6: i64 = 100;

/// HIP-4 price law: at most this many significant figures. BIN15 H90 G2
/// (2026-10-16): mainnet accepted and filled IoCs at 0.99909 and 0.99911
/// (doc 33 §9 / §9b), so the 1e-4 tick the grid carried until then was
/// stricter than the venue and skipped real asks. In ×1e6 units a price at
/// or above 0.1 is a multiple of 10 (1e-5); below 0.1 every ×1e6 value
/// already has at most five figures.
pub const PX_SIG_FIGS: u32 = 5;

/// The ×1e6 level at and above which a price has six digits, so its last
/// one must be 0 to keep [`PX_SIG_FIGS`] figures.
const PX_SIX_DIGITS_1E6: i64 = 100_000;

/// HIP-4 lot ×1e6: one whole contract.
pub const GRID_LOT_1E6: i64 = 1_000_000;

/// Lowest binary price ×1e6 (0.00001).
pub const GRID_PX_MIN_1E6: i64 = 10;

/// Highest binary price ×1e6 (0.99999).
pub const GRID_PX_MAX_1E6: i64 = 999_990;

/// Minimum order notional ×1e6 ($1). MEASURED, not read from a doc:
/// the venue refuses `Order must have minimum value of 1 USDC.` (testnet,
/// 2026-09-15 size 1 @ 0.68 and 2026-09-19 3 @ 0.30; 4 @ 0.30 = $1.20
/// rests). The $10 this carried until 2026-09-19 was the PERPS rule
/// copied from third-party docs — `docs/risk-policy.md` "E4 phase D".
pub const GRID_MIN_NOTIONAL_1E6: i128 = 1_000_000;

/// One whole payout ×1e6 — a binary settles at 0 or at this.
pub const ONE_1E6: i64 = 1_000_000;

/// Family kind: a 15-minute rolling binary (`out:<COIN>:15m`).
pub const FAMILY_OUT_15M: u8 = 0;
/// Family kind: a native recurring daily binary (`native:<COIN>:1d`).
pub const FAMILY_NATIVE_DAILY: u8 = 1;

/// `client_oid` bit 48: the side (0 Yes, 1 No).
const OID_SIDE_SHIFT: u32 = 48;
/// `client_oid` bits 32..48: the family index.
const OID_FAMILY_SHIFT: u32 = 32;
/// `client_oid` bits 0..32: the outcome id.
///
/// **The engine-wide instance convention**, not this member's private
/// layout — `exec-hyperliquid` reads these bits to ask its asset table
/// which instance an order names (LAW E-4), and it cannot see this
/// crate. Defined in `core-types` next to the field itself so the two
/// readers cannot drift apart.
use core_types::OID_INSTANCE_MASK as OID_OUTCOME_MASK;

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
    /// BIN15 P2 (F2): the notional this leg BOOKED against the caps at
    /// submit ×1e6.
    ///
    /// Kept on the leg rather than recomputed from `px × qty`, because
    /// the credit has to return exactly what was taken: a re-derivation
    /// that floors differently by one micro-dollar leaks cap room in one
    /// direction forever.
    pub booked_notional_1e6: i64,
    /// `1` Yes leg, `0` No leg.
    pub is_yes: u8,
    /// [`Side`] as a byte.
    pub side: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 14],
}

// One cache line, so the family's scan touches one line per leg.
const _: () = assert!(::core::mem::size_of::<PendingLeg>() == 64);

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
    /// Explicit padding — always zero.
    pub _pad: [u8; 2],
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
    /// Notional booked against `cap_instance_usd_1e6` ×1e6.
    pub notional_instance_1e6: i64,
    /// BIN15 O6: the standardised distance the last reprice priced at
    /// ×1e6 (the pricer's `d`, already clamped).
    pub d_1e6: i64,
    /// BIN15 O6: σ√τ ×1e9 behind that `d` — a small denominator is
    /// what an overconfident forecast looks like.
    pub den_1e9: i64,
    /// BIN15 O6: the mark the last reprice used ×1e6.
    pub mark_1e6: i64,
    /// BIN15 O6: the pricing horizon the last reprice used, ns (BIN15 S3:
    /// `price::pricing_horizon_ns` — the variance-time left in the
    /// settlement TWAP, not the time to expiry).
    pub last_tau_ns: u64,
    /// BIN15 S3 (ruling O-2): `∫ mark du` over `[T − W, twap_last_ts]`
    /// (mark ×1e6 · wall ns) — the part of the venue's settlement average
    /// already behind, integrated from the underlying's marks with the
    /// last mark carried forward (`core_types::binary_twap_segment`, the
    /// arithmetic the harness settles with).
    ///
    /// The three S3 fields cost the struct one cache line (448 → 512 B)
    /// and sit together in it, so a fold writes one line per family.
    pub twap_sum: i128,
    /// BIN15 S3: the wall instant `twap_sum` is integrated up to; `0` =
    /// nothing yet.
    pub twap_last_ts: u64,
    /// BIN15 S3: `1` once this instance's settlement window has a hole
    /// the member cannot fill — its open was not covered by a mark this
    /// family saw in force, or a piece spanning it outlasted
    /// `core_types::BINARY_SETTLE_MARK_GAP_MAX_NS`. The evidence law the
    /// harness settles with (LAW E-11); a running average with a hole in
    /// it is not the venue's, so the family HOLDS for the rest of the
    /// window.
    pub twap_gap: u8,
}

// Six cache lines since BIN15 H90 G3 retired the old arms' fields (the Arm B
// quotes, the coverage entry's S5 run and counterfactuals). A field added
// here that crosses into a seventh is a decision, not an accident.
const _: () = assert!(::core::mem::size_of::<FamilyState>() == 384);

impl Default for FamilyState {
    fn default() -> Self {
        Self {
            live: BinarySpec::default(),
            sym_yes: SYMBOL_ID_NONE,
            sym_no: SYMBOL_ID_NONE,
            underlying: 0,
            kind: FAMILY_OUT_15M,
            _pad: [0; 2],
            touch_yes: TouchState::default(),
            touch_no: TouchState::default(),
            p_hat_1e6: 0,
            p_raw_1e6: 0,
            p_ts_ns: 0,
            pos_yes_1e6: 0,
            pos_no_1e6: 0,
            pend_take: PendingLeg::default(),
            notional_instance_1e6: 0,
            d_1e6: 0,
            den_1e9: 0,
            mark_1e6: 0,
            last_tau_ns: 0,
            twap_sum: 0,
            twap_last_ts: 0,
            twap_gap: 0,
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
        self.notional_instance_1e6 = 0;
        // BIN15 O6: the diagnostics describe the instance that is
        // going away, so they go with it — a stale `d` read against
        // the successor's strike is worse than no reading at all.
        self.d_1e6 = 0;
        self.den_1e9 = 0;
        self.mark_1e6 = 0;
        self.last_tau_ns = 0;
        // BIN15 S3: and its own settlement average.
        self.twap_sum = 0;
        self.twap_last_ts = 0;
        self.twap_gap = 0;
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
    /// The last window before expiry in which the S-law pricer does not
    /// price, ns (the late law's floor is `late.tau_min_ns`).
    pub tail_refuse_ns: u64,
    /// Order TTL, ns — the late take's deadline (an unfilled take is
    /// written off when it passes).
    pub requote_ttl_ns: u64,
    /// Notional cap per instance ×1e6.
    pub cap_instance_usd_1e6: i64,
    /// Notional cap per UTC day ×1e6.
    pub cap_day_usd_1e6: i64,
    /// BIN15 P0 (F4): the oldest an underlying mark may be and still
    /// price a binary, ns.
    ///
    /// Hyperliquid prints a mark every 1-3 s. Past this age the mark is
    /// not a price, it is a memory: the binary's own book keeps tracking
    /// reality while `p_hat` is pinned to a number the tape has left
    /// behind, and the ledger then records the difference between the
    /// venue and our own lag. The gate holds the re-price
    /// ([`Hold::MarkStale`]) rather than widening an edge, because
    /// ABSENT DATA HOLDS.
    pub mark_stale_ns: u64,
    /// `ln σ̂` offset per UTC hour ×1e9; all zero = no hour table.
    pub hour_ln_off_1e9: [i64; 24],
    /// Variance-ratio scale on σ̂ ×1e9; `1e9` = no scaling.
    pub scale_1e9: i64,
    /// BIN15 H90: the late-window entry's law. `late.enabled == 0` = the
    /// member exactly as it was.
    pub late: late::LateParams,
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
            tail_refuse_ns: 10_000_000_000,
            requote_ttl_ns: 1_000_000_000,
            cap_instance_usd_1e6: 1_000_000_000,
            cap_day_usd_1e6: 5_000_000_000,
            // 5 s = ~2-5 missed HL mark prints. Long enough that an
            // ordinary jitter does not hold the member, short enough
            // that a dead feed cannot price a 15-minute binary.
            mark_stale_ns: 5_000_000_000,
            hour_ln_off_1e9: [0; 24],
            scale_1e9: 1_000_000_000,
            late: late::LateParams::default(),
        }
    }
}

/// BIN15 H90: one family's late-window entry — the run, the in-flight id,
/// the counterfactuals and what the entry was priced at. Its own cache
/// line per family, beside [`FamilyState`] (which stays eight lines).
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LateState {
    /// `1` once this instance's late entry was SUBMITTED.
    pub covered: u8,
    /// The persistence run (consecutive distinct polls that passed).
    pub hits: u8,
    /// The run's side, `1` Yes / `0` No.
    pub yes: u8,
    /// The P1b counterfactual's side (valid while `ff_ts != 0`).
    pub ff_yes: u8,
    /// The production-pricer counterfactual's side (valid while
    /// `ctl_ts != 0`).
    pub ctl_yes: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 3],
    /// The favoured leg's snapshot (`TouchState::ts_ns`) the run last
    /// counted.
    pub last_touch_ts: u64,
    /// The in-flight late IoC's client id; `0` = none. A venue refusal of
    /// it (a gateway's late `REJECTED`) lets the entry retry.
    pub oid: u64,
    /// When P1b's test FIRST held on this instance (persist 1), engine ns;
    /// `0` = never. Logged, never traded.
    pub ff_ts: u64,
    /// When the PRODUCTION p̂ first passed the same test, engine ns; `0` =
    /// never (or it never priced in the window). Logged, never traded.
    pub ctl_ts: u64,
    /// The favoured ask at `ff_ts` ×1e6.
    pub ff_px_1e6: i32,
    /// The production counterfactual's favoured ask at `ctl_ts` ×1e6.
    pub ctl_px_1e6: i32,
    /// P1b's belief on the side the entry bought ×1e6.
    pub entry_p_1e6: i32,
    /// σ behind that belief ×1e9 per √s.
    pub entry_sigma_1e9: u32,
    /// The production p̂ on that side at the entry ×1e6, or `-1` when it
    /// did not price at that reprice.
    pub entry_ctl_p_1e6: i32,
    /// Explicit padding — always zero.
    pub _pad2: [u8; 4],
}

const _: () = assert!(::core::mem::size_of::<LateState>() == 64);

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
    /// BIN15 H90: per-family late-entry state.
    late_fam: [LateState; BIN15_MAX_FAMILIES],
    /// BIN15 H90: per-underlying 1-s grids (≈ 9.4 KiB each), boxed at boot.
    late_grid: Box<[late::LateGrid; BIN15_MAX_UNDERLYINGS]>,
    /// BIN15 H90: the late Φ table, boxed at boot.
    late_lut: Box<late::LateLut>,
    /// `1` once [`Bin15Strategy::configure_late`] installed the table.
    late_lut_set: u8,
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
            late_fam: [LateState::default(); BIN15_MAX_FAMILIES],
            late_grid: Box::new(core::array::from_fn(|_| late::LateGrid::new())),
            late_lut: Box::new(late::LateLut::flat()),
            late_lut_set: 0,
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
        if params.cap_instance_usd_1e6 <= 0 || params.cap_day_usd_1e6 <= 0 {
            return Err(StrategyError::Config("bin15: a cap is not positive"));
        }
        if params.requote_ttl_ns == 0 {
            return Err(StrategyError::Config("bin15: requote_ttl_ns is zero"));
        }
        let lp = params.late;
        if lp.enabled > 1
            || lp.persist_polls == 0
            || lp.tau_min_ns < 1_000_000_000
            || lp.tau_min_ns >= lp.tau_max_ns
            || lp.tau_max_ns > 60_000_000_000
            || !(ONE_1E6 / 2..ONE_1E6).contains(&lp.p_min_1e6)
            || lp.e_1e6 < GRID_TICK_1E6
            || lp.e_1e6 >= ONE_1E6 / 2
            // The stake is bounded by the cap only where it is spent: an
            // artifact with the law off and a cap under $100 is today's
            // member (core-config clamps the absent default the same way).
            || (lp.enabled == 1
                && (lp.usd_1e6 < GRID_MIN_NOTIONAL_1E6 as i64
                    || lp.usd_1e6 > params.cap_instance_usd_1e6))
            || !(250..=4_000).contains(&lp.sigma_k_1e3)
        {
            return Err(StrategyError::Config(
                "bin15: a late-entry parameter is outside core-config's bounds",
            ));
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

    /// BIN15 H90: install the late Φ table (boot only). Required before
    /// `on_start` when `params.late.enabled == 1`; harmless otherwise.
    pub fn configure_late(&mut self, lut: Box<late::LateLut>) {
        self.late_lut = lut;
        self.late_lut_set = 1;
    }

    /// BIN15 H90: one family's late-entry state (observability, the harness).
    #[inline]
    #[must_use]
    pub fn late_state(&self, idx: usize) -> Option<&LateState> {
        if idx >= self.params.n_families {
            None
        } else {
            Some(&self.late_fam[idx])
        }
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

    /// BIN15 P2 (F2): notional booked against the DAY cap ×1e6.
    ///
    /// Read by tests and by anything that wants to know whether the cap
    /// is measuring exposure or counting submissions — the difference
    /// between the two is invisible from the counters alone.
    #[inline]
    #[must_use]
    pub const fn day_notional_1e6(&self) -> i64 {
        self.day_notional_1e6
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
    /// A roll or a settle drops the in-flight take: credit the cap room its
    /// unfilled part had reserved.
    fn release_pendings(&mut self, idx: usize) {
        let take = self.fam[idx].pend_take;
        if take.live() {
            self.credit_unfilled(idx, &take);
        }
    }

    /// Bind a new instance to a family slot.
    fn bind(&mut self, idx: usize, spec: BinarySpec) {
        debug_assert!(idx < self.params.n_families);
        debug_assert!(spec.outcome != 0);
        self.release_pendings(idx);
        self.fam[idx].clear_instance();
        // BIN15 H90: the successor gets its own late entry.
        self.late_fam[idx] = LateState::default();
        self.fam[idx].live = spec;
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
    ///
    /// BIN15 S1: the forecast pair this closes is the ARM→EXPIRY
    /// realised variance (`realised_since_arm_1e9`, armed at `bind` for
    /// the instance's `expiry`), while the payoff itself is decided on
    /// the venue's TWAP over `[T − twap, T]` (LAW E-11). Scoring σ̂
    /// against the payoff's own window instead is a follow-up
    /// MEASUREMENT, not a change — σ̂'s level was measured honest (vault
    /// doc 27 §4) — so the observation is left exactly as it was.
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
        self.release_pendings(idx);
        self.fam[idx].clear_instance();
        self.late_fam[idx] = LateState::default();
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
        // ONE codec — `core_types::unpack_roll_seq` — and ONE reading
        // of the kind byte, the strict one. This function used to
        // restate the layout inline and compare the whole byte to 1,
        // so a kind byte no packer of ours writes (`0x03`) read as
        // CREATED and `bind()` flattened a live instance, while the
        // exchange arm read the same frame as SETTLED (E7 review,
        // 2026-09-19). A malformed frame is refused, not guessed.
        let seq = event.venue_seq;
        let (outcome, twap_s, family, _masked) = core_types::unpack_roll_seq(seq);
        let twap_s = u64::from(twap_s);
        let Some(settled) = core_types::roll_kind_strict(seq) else {
            self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
            return;
        };
        if family >= self.params.n_families {
            self.counters.spec_refused = self.counters.spec_refused.wrapping_add(1);
            return;
        }
        if settled {
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
        let prev_mark = self.marks[u].mark_1e6;
        let wall_now = self.bar.anchor.wall_of(now);
        // BIN15 H90: the late grid writes the seconds before this mark with
        // the one it replaces, then takes it. Off = untouched.
        if self.params.late.enabled == 1 {
            self.late_grid[u].on_mark(wall_now, mark_1e6);
        }
        // BIN15 S3: the mark being replaced has been the mark since its
        // own stamp; fold that interval into every live window on this
        // underlying before it is gone.
        if prev_mark > 0 {
            let since = self.bar.anchor.wall_of(self.marks[u].ts_ns);
            self.fold_twap(u, prev_mark, since, wall_now);
        }
        let minute = self.bar.bar_id(now);
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
            self.refresh_sigma(u, wall_now);
        }
        self.marks[u].mark_1e6 = mark_1e6;
        self.marks[u].ts_ns = now;
    }

    /// BIN15 S3: fold `px_1e6`, the mark in force over `[since_wall,
    /// to_wall)`, into the running settlement average of every live
    /// TWAP-settled family on underlying `u` whose window `[T − W, T]` the
    /// piece reaches. Integer, no allocation: one
    /// `core_types::binary_twap_segment` per family — the arithmetic the
    /// harness settles with — and the interval already integrated
    /// (`twap_last_ts`) is never counted twice.
    ///
    /// The evidence is judged as the harness judges it
    /// (`cli::backtest::binary::settle_reference_1e6`): the first piece
    /// into a window must start AT or before its open (a mark this family
    /// saw in force when the minute began), and no piece spanning the
    /// window may be longer than `core_types::BINARY_SETTLE_MARK_GAP_MAX_NS`
    /// — measured over the WHOLE piece, the part before the open included.
    /// A breach sets `twap_gap` for the rest of the instance.
    #[inline]
    fn fold_twap(&mut self, u: usize, px_1e6: i64, since_wall: u64, to_wall: u64) {
        // `configure` already refuses more than `BIN15_MAX_FAMILIES`; the
        // `min` states it where the compiler can see it (no bounds check).
        let n = self.params.n_families.min(BIN15_MAX_FAMILIES);
        let mut f = 0usize;
        while f < n {
            let fam = &mut self.fam[f];
            f += 1;
            if fam.underlying as usize != u || !fam.is_live() || fam.live.twap_ns == 0 {
                continue;
            }
            let close = fam.live.expiry_ns;
            let open = core_types::binary_settle_open_ns(close, fam.live.twap_ns);
            if to_wall <= open || since_wall >= close || to_wall <= since_wall {
                continue;
            }
            let upto = if to_wall < close { to_wall } else { close };
            let head_unseen = fam.twap_last_ts == 0 && since_wall > open;
            let too_long = upto - since_wall > core_types::BINARY_SETTLE_MARK_GAP_MAX_NS;
            fam.twap_gap |= u8::from(head_unseen | too_long);
            let from = if fam.twap_last_ts > since_wall { fam.twap_last_ts } else { since_wall };
            fam.twap_sum += core_types::binary_twap_segment(px_1e6, from, to_wall, open, close).0;
            if upto > fam.twap_last_ts {
                fam.twap_last_ts = upto;
            }
        }
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
// the re-price and the late law
// ---------------------------------------------------------------

/// Why a re-price produced nothing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Hold {
    /// No live instance on this slot.
    Dormant,
    /// Inside `tail_refuse_ns`, or past expiry.
    Tail,
    /// The underlying mark or the forecast is absent — or, inside a
    /// settlement window, the running average is (BIN15 S3: `twap_gap`).
    Stale,
    /// BIN15 P0 (F4): the underlying mark exists but is older than
    /// `mark_stale_ns`. Its own variant, not `Stale`, because "we never
    /// had a mark" and "our mark is from a minute ago" are different
    /// defects and only the second one is silent.
    MarkStale,
}

impl Bin15Strategy {
    /// Re-price one family (the S-law counterfactual), then run the late
    /// law.
    ///
    /// Called on every `Mark` of the family's underlying and on every
    /// touch change of either leg — the two things that can move the
    /// decision. Never on a timer: a re-price with no new evidence is
    /// the same number.
    fn reprice<C: Ctx>(&mut self, ctx: &mut C, idx: usize, now: NsTs) {
        let res = self.try_reprice(idx, now);
        // BIN15 H90: the late law has its own window and its own holds; it
        // runs whatever the S-law pricer did, after it, so a fresh p̂ (the
        // production counterfactual) is this reprice's.
        if self.params.late.enabled == 1 {
            self.late_reprice(ctx, idx, now);
        }
        let Err(reason) = res else {
            return;
        };
        match reason {
            Hold::Dormant => {}
            Hold::Tail => self.counters.skipped_tail = self.counters.skipped_tail.wrapping_add(1),
            Hold::Stale => {
                self.counters.skipped_stale = self.counters.skipped_stale.wrapping_add(1);
            }
            Hold::MarkStale => {
                self.counters.skipped_mark_stale =
                    self.counters.skipped_mark_stale.wrapping_add(1);
            }
        }
    }

    /// The pricer proper. `Err` names the gate that held.
    fn try_reprice(&mut self, idx: usize, now: NsTs) -> Result<(), Hold> {
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
        // BIN15 P0 (F4). One u64 compare, before anything is computed
        // from the mark. `now` and `MarkState::ts_ns` are both ENGINE
        // ns (`on_mark` stamps the same clock this is called on), so
        // the subtraction needs no anchor. `saturating_sub` because a
        // seeded-but-unmarked underlying has `ts_ns == 0` and a member
        // that has not yet seen a mark must hold, not divide.
        if now.saturating_sub(self.marks[u].ts_ns) > self.params.mark_stale_ns {
            return Err(Hold::MarkStale);
        }
        // BIN15 S3 (ruling O-2): the venue settles on the mark's TWAP
        // over `[T − W, T]` (LAW E-11). While that window lies ahead the
        // pricer's horizon is `τ − 2W/3`; inside it the part of the
        // average already behind (`twap_sum`, plus the current mark
        // carried from its stamp to now) moves the MEAN and only
        // `τ³/(3W²)` of variance is left (`price::fair_value_twap`).
        // A window with a hole in its evidence — `twap_gap`, an open this
        // family never saw a mark in force at, or the current mark carried
        // past the gap bound — has no known average: ABSENT DATA HOLDS.
        let to_expiry = expiry - wall;
        let twap = self.fam[idx].live.twap_ns;
        let a_known: i128 = if twap > 0 && to_expiry < twap {
            let open = core_types::binary_settle_open_ns(expiry, twap);
            let mark_wall = self.bar.anchor.wall_of(self.marks[u].ts_ns);
            let fam = &self.fam[idx];
            if fam.twap_gap != 0
                || (fam.twap_last_ts == 0 && mark_wall > open)
                || wall.saturating_sub(mark_wall) > core_types::BINARY_SETTLE_MARK_GAP_MAX_NS
            {
                return Err(Hold::Stale);
            }
            let from = if fam.twap_last_ts > mark_wall { fam.twap_last_ts } else { mark_wall };
            fam.twap_sum + core_types::binary_twap_segment(mark, from, wall, open, expiry).0
        } else {
            0
        };
        let Some(fair) = price::fair_value_twap(
            &self.lut,
            mark,
            self.fam[idx].live.strike_1e6,
            to_expiry,
            twap,
            a_known,
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
        self.fam[idx].last_tau_ns = fair.horizon_ns;
        self.counters.reprices = self.counters.reprices.wrapping_add(1);
        Ok(())
    }

    /// BIN15 H90 — the LATE-WINDOW ENTRY (vault doc 33 §1).
    ///
    /// Inside `[late_tau_min, late_tau_max)` to expiry: advance the
    /// underlying's 1-s grid, price with [`late::late_fair`], record both
    /// counterfactuals (P1b's own and the production p̂'s, persist 1), then
    /// — once per instance — buy the favoured side at its ask when the
    /// belief clears `p_min` and the ask sits `e` under it on
    /// `persist_polls` consecutive snapshots of that leg (the S5 poll),
    /// sized at the whole resting ask up to `usd` and the caps. Not held
    /// by `tail_refuse_ns`: the window floor is its own. Everything else binds: one pending take, the grid, the caps.
    fn late_reprice<C: Ctx>(&mut self, ctx: &mut C, idx: usize, now: NsTs) {
        let lp = self.params.late;
        if self.fam[idx].kind != FAMILY_OUT_15M || !self.fam[idx].is_live() {
            return;
        }
        let wall = self.bar.anchor.wall_of(now);
        let expiry = self.fam[idx].live.expiry_ns;
        if wall >= expiry {
            return;
        }
        let tte = expiry - wall;
        if tte < lp.tau_min_ns || tte >= lp.tau_max_ns {
            return;
        }
        let u = self.fam[idx].underlying as usize;
        self.late_grid[u].advance_to(wall / late::SEC_NS);
        self.counters.late_reprices = self.counters.late_reprices.wrapping_add(1);
        let fair = match late::late_fair(
            &self.late_grid[u],
            &self.late_lut,
            self.fam[idx].live.strike_1e6,
            expiry,
            wall,
            lp.sigma_k_1e3,
        ) {
            Ok(f) => f,
            Err(_) => {
                self.counters.late_skipped_stale = self.counters.late_skipped_stale.wrapping_add(1);
                return;
            }
        };
        let want_yes = fair.p_yes_1e6 >= ONE_1E6 / 2;
        let p_side = if want_yes { fair.p_yes_1e6 } else { ONE_1E6 - fair.p_yes_1e6 };
        let touch = if want_yes { self.fam[idx].touch_yes } else { self.fam[idx].touch_no };
        let px = touch.touch.ask_1e6;
        let book_ok = touch.stale == 0 && px > 0 && px < ONE_1E6 && touch.touch.ask_qty_1e6 > 0;
        let ok = book_ok && p_side >= lp.p_min_1e6 && px <= p_side - lp.e_1e6;
        // The production pricer's counterfactual, where it priced at THIS
        // reprice (inside the tail it does not).
        let ctl_p = if self.fam[idx].p_ts_ns == now { self.fam[idx].p_hat_1e6 } else { -1 };
        let ctl_side_p = if ctl_p < 0 {
            -1
        } else if want_yes {
            ctl_p
        } else {
            ONE_1E6 - ctl_p
        };
        let st = &mut self.late_fam[idx];
        if ok && st.ff_ts == 0 {
            st.ff_ts = now;
            st.ff_px_1e6 = px as i32;
            st.ff_yes = u8::from(want_yes);
        }
        if ctl_p >= 0 && st.ctl_ts == 0 {
            let c_yes = ctl_p >= ONE_1E6 / 2;
            let c_p = if c_yes { ctl_p } else { ONE_1E6 - ctl_p };
            let c_touch = if c_yes { self.fam[idx].touch_yes } else { self.fam[idx].touch_no };
            let c_px = c_touch.touch.ask_1e6;
            if c_touch.stale == 0
                && c_px > 0
                && c_touch.touch.ask_qty_1e6 > 0
                && c_p >= lp.p_min_1e6
                && c_px <= c_p - lp.e_1e6
            {
                st.ctl_ts = now;
                st.ctl_px_1e6 = c_px as i32;
                st.ctl_yes = u8::from(c_yes);
            }
        }
        if st.covered != 0 || self.fam[idx].pend_take.live() {
            return;
        }
        if !book_ok {
            self.counters.late_skipped_book = self.counters.late_skipped_book.wrapping_add(1);
            return;
        }
        // The S5 poll on the favoured leg's own snapshots.
        let yes = u8::from(want_yes);
        let pass = u8::from(ok);
        let same_side = yes == st.yes;
        let fresh = touch.ts_ns != st.last_touch_ts || !same_side;
        let run = if same_side { st.hits } else { 0 };
        let poll = lp.persist_polls == 1 || (fresh && now == touch.ts_ns);
        let next = if !poll {
            run
        } else if fresh {
            run.saturating_add(1) * pass
        } else {
            run.max(pass)
        };
        if poll {
            st.hits = next;
            st.yes = yes;
            st.last_touch_ts = touch.ts_ns;
        }
        if !ok {
            self.counters.late_skipped_price = self.counters.late_skipped_price.wrapping_add(1);
            return;
        }
        if next < lp.persist_polls {
            self.counters.late_skipped_persist =
                self.counters.late_skipped_persist.wrapping_add(1);
            return;
        }
        let want = (lp.usd_1e6 as i128 * 1_000_000) / px as i128;
        let want = i64::try_from(want).unwrap_or(i64::MAX);
        let qty = want.min(touch.touch.ask_qty_1e6).min(self.cap_room_1e6(idx, px));
        if qty < GRID_LOT_1E6 {
            self.counters.late_skipped_cap = self.counters.late_skipped_cap.wrapping_add(1);
            return;
        }
        let before = self.counters.takes_submitted;
        self.emit_take(ctx, idx, want_yes, px, qty, now);
        if self.counters.takes_submitted != before {
            let oid = self.fam[idx].pend_take.oid;
            let st = &mut self.late_fam[idx];
            st.covered = 1;
            st.oid = oid;
            st.entry_p_1e6 = p_side as i32;
            st.entry_sigma_1e9 = u32::try_from(fair.sigma_1e9).unwrap_or(u32::MAX);
            st.entry_ctl_p_1e6 = ctl_side_p as i32;
            self.counters.late_submitted = self.counters.late_submitted.wrapping_add(1);
        }
    }

    /// Room left under both notional caps ×1e6, at `px_1e6` per
    /// contract. `0` = no room.
    fn cap_room_1e6(&self, idx: usize, px_1e6: i64) -> i64 {
        if px_1e6 <= 0 {
            return 0;
        }
        let inst = self.params.cap_instance_usd_1e6 - self.fam[idx].notional_instance_1e6.max(0);
        let day = self.params.cap_day_usd_1e6 - self.day_notional_1e6.max(0);
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
    /// refuses off-grid orders outright (O3), so a take that emitted
    /// one would be counted as a submit and never fill — a silent
    /// hole in the member's own accounting. Public for one reader: the
    /// cli holds it in agreement with the gateway arm's `OutcomeRules`
    /// (XH4i-3a), which refuses what is off the grid and never rounds.
    #[inline]
    #[must_use]
    pub fn on_grid(px_1e6: i64, qty_1e6: i64) -> bool {
        (px_1e6 < PX_SIX_DIGITS_1E6 || px_1e6 % 10 == 0)
            && qty_1e6 % GRID_LOT_1E6 == 0
            && (GRID_PX_MIN_1E6..=GRID_PX_MAX_1E6).contains(&px_1e6)
            && (px_1e6 as i128 * qty_1e6 as i128) / 1_000_000 >= GRID_MIN_NOTIONAL_1E6
    }

    /// The member's order id: sequence, side, family and outcome. Bit 63
    /// (the retired null-arm flag, BIN15 H90 G3) is always 0.
    fn next_oid(&mut self, idx: usize, is_yes: bool) -> u64 {
        self.oid_seq = self.oid_seq.wrapping_add(1);
        let side = u64::from(!is_yes) << OID_SIDE_SHIFT;
        let fam = (idx as u64 & 0xFFFF) << OID_FAMILY_SHIFT;
        let seq = (self.oid_seq & 0x3FFF) << 49;
        seq | side | fam | (u64::from(self.fam[idx].live.outcome) & OID_OUTCOME_MASK)
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
            // E5: every refusal is the same fact to this member —
            // the dispatcher did not take the order. Spelled out
            // rather than `_` so a new `SubmitErr` variant forces
            // this decision to be made again instead of silently
            // joining the drop arm.
            Err(
                SubmitErr::RingFull
                | SubmitErr::Unsupported
                | SubmitErr::NoSuchOrder
                | SubmitErr::Refused,
            ) => {
                self.orders_dropped = self.orders_dropped.wrapping_add(1);
                false
            }
        }
    }

    /// Emit one late-window IoC BUY (the only order this member sends since
    /// BIN15 H90 G3).
    fn emit_take<C: Ctx>(
        &mut self,
        ctx: &mut C,
        idx: usize,
        is_yes: bool,
        px_1e6: i64,
        qty_1e6: i64,
        now: NsTs,
    ) {
        if qty_1e6 <= 0 {
            self.counters.skipped_cap = self.counters.skipped_cap.wrapping_add(1);
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
        if !self.submit(ctx, sym, Side::Bid, ORDER_KIND_IOC, px_1e6, qty, oid, now) {
            return;
        }
        let notional = ((px_1e6 as i128 * qty as i128) / 1_000_000) as i64;
        self.fam[idx].notional_instance_1e6 =
            self.fam[idx].notional_instance_1e6.saturating_add(notional);
        self.day_notional_1e6 = self.day_notional_1e6.saturating_add(notional);
        self.counters.takes_submitted = self.counters.takes_submitted.wrapping_add(1);
        self.fam[idx].pend_take = PendingLeg {
            oid,
            deadline_ns: now.saturating_add(self.params.requote_ttl_ns),
            px_1e6,
            qty_1e6: qty,
            filled_1e6: 0,
            booked_notional_1e6: notional,
            is_yes: u8::from(is_yes),
            side: Side::Bid as u8,
            _pad: [0; 14],
        };
    }

    /// Book one fill against the in-flight take. A settlement print is
    /// counted apart; a fill no pending leg owns is counted unknown.
    fn book_fill(&mut self, fill: &core_types::Fill) {
        let qty = fill.qty.raw();
        if qty <= 0 {
            return;
        }
        if fill.is_settlement() {
            self.counters.settlement_fills = self.counters.settlement_fills.wrapping_add(1);
            return;
        }
        let mut i = 0usize;
        while i < self.params.n_families {
            if self.fam[i].pend_take.oid == fill.order_id {
                let is_yes = self.fam[i].pend_take.is_yes == 1;
                let first = self.fam[i].pend_take.filled_1e6 == 0;
                self.apply_position(i, is_yes, qty);
                let filled = self.fam[i].pend_take.filled_1e6.saturating_add(qty);
                self.fam[i].pend_take.filled_1e6 = filled;
                if filled >= self.fam[i].pend_take.qty_1e6 {
                    let leg = self.fam[i].pend_take;
                    self.fam[i].pend_take = PendingLeg::default();
                    self.credit_unfilled(i, &leg);
                }
                if first {
                    self.counters.takes_filled = self.counters.takes_filled.wrapping_add(1);
                    if self.late_fam[i].oid == fill.order_id {
                        self.counters.late_filled = self.counters.late_filled.wrapping_add(1);
                    }
                }
                self.counters.fills = self.counters.fills.wrapping_add(1);
                return;
            }
            i += 1;
        }
        self.counters.unknown_fills = self.counters.unknown_fills.wrapping_add(1);
    }

    /// BIN15 P2 (F2): give back the cap room a leg no longer occupies.
    ///
    /// A cap is a limit on EXPOSURE. Booking at submit is right — eight
    /// intents in flight can all fill — but a booking that is never
    /// released turns the cap into a SUBMISSIONS ODOMETER: twenty
    /// unfilled takes would exhaust the day's room without the member
    /// ever holding a contract, and every later instance is then
    /// refused. That refusal is what
    /// censors the calibration ledger — the instances that never traded
    /// are exactly the ones the gate needed.
    ///
    /// The credit is the booked notional scaled by the UNFILLED share,
    /// floored: `booked · (qty − filled) / qty`. Flooring returns at
    /// most what was taken, so repeated partial credits can never hand
    /// back more room than the leg ever occupied.
    #[inline]
    fn credit_unfilled(&mut self, idx: usize, leg: &PendingLeg) {
        if leg.booked_notional_1e6 <= 0 || leg.qty_1e6 <= 0 {
            return;
        }
        let unfilled = leg.qty_1e6.saturating_sub(leg.filled_1e6).max(0);
        debug_assert!(unfilled <= leg.qty_1e6);
        if unfilled == 0 {
            return;
        }
        let back = ((leg.booked_notional_1e6 as i128 * unfilled as i128)
            / leg.qty_1e6 as i128) as i64;
        self.fam[idx].notional_instance_1e6 =
            (self.fam[idx].notional_instance_1e6 - back).max(0);
        self.day_notional_1e6 = (self.day_notional_1e6 - back).max(0);
    }

    /// The venue (or the dispatcher) refused the take: release its cap room;
    /// a refused LATE entry may try again inside its window.
    fn on_rejected(&mut self, oid: u64) {
        let mut i = 0usize;
        while i < self.params.n_families {
            if self.fam[i].pend_take.live() && self.fam[i].pend_take.oid == oid {
                let leg = self.fam[i].pend_take;
                self.fam[i].pend_take = PendingLeg::default();
                self.credit_unfilled(i, &leg);
                if leg.filled_1e6 == 0 {
                    self.counters.takes_unfilled = self.counters.takes_unfilled.wrapping_add(1);
                }
                if self.late_fam[i].oid == oid {
                    self.late_fam[i].oid = 0;
                    self.late_fam[i].covered = 0;
                }
                return;
            }
            i += 1;
        }
    }

    /// A take is a BUY: the member never sells (it holds to settlement).
    fn apply_position(&mut self, idx: usize, is_yes: bool, qty_1e6: i64) {
        let pos = if is_yes {
            &mut self.fam[idx].pos_yes_1e6
        } else {
            &mut self.fam[idx].pos_no_1e6
        };
        *pos = pos.saturating_add(qty_1e6);
    }

    /// A take past its TTL is gone at the venue (an IoC): release its room.
    fn sweep_pendings(&mut self, now: NsTs) {
        let mut i = 0usize;
        while i < self.params.n_families {
            if self.fam[i].pend_take.live() && now >= self.fam[i].pend_take.deadline_ns {
                let leg = self.fam[i].pend_take;
                self.fam[i].pend_take = PendingLeg::default();
                self.credit_unfilled(i, &leg);
                if leg.filled_1e6 == 0 {
                    self.counters.takes_unfilled =
                        self.counters.takes_unfilled.wrapping_add(1);
                }
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
                // Rounded UP: inside the settlement window the horizon is
                // under a second for the last ~22 s (BIN15 S3), and `0`
                // is the view's "never priced".
                tau_s: u32::try_from(self.fam[i].last_tau_ns.div_ceil(1_000_000_000))
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
        if self.params.late.enabled == 1 && self.late_lut_set == 0 {
            return Err(StrategyError::Config(
                "bin15: the late entry is on without its Φ table (configure_late)",
            ));
        }
        Ok(())
    }

    /// A tick on one of the binary legs updates that leg's touch and
    /// re-prices. Ticks on anything else are ignored — the MARK is the
    /// price source for the S-law pricer — with ONE exception (BIN15 H90):
    /// with the late entry on, the underlying's own BBO feeds the late
    /// grid's nowcast, and nothing else.
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        if self.configured == 0 {
            return;
        }
        let Some((idx, is_yes)) = self.family_of_sym(tick.sym) else {
            // BIN15 H90: the underlying's own book is part of the LATE law
            // (the nowcast), and of nothing else. No reprice: a perp tick is
            // not a poll.
            if self.params.late.enabled == 1 && !tick.is_stale() {
                if let Some(u) = self.underlying_of_sym(tick.sym) {
                    let bid = tick.bid_px.raw();
                    let ask = tick.ask_px.raw();
                    if bid > 0 && ask > 0 {
                        let wall = self.bar.anchor.wall_of(tick.ts_ns);
                        self.late_grid[u].on_perp(wall, bid + ask);
                    }
                }
            }
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
            // flag keeps the late law off them.
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

    /// XH4i-6 (Q1): a venue's verdict reported late — only a gateway arm
    /// sends this member one. A `REJECTED` is the blocking arm's `Err`
    /// (`on_rejected`): the take's room comes back. The member rests
    /// nothing since H90 G3, so every other kind changes nothing here.
    fn on_order_event<C: Ctx>(&mut self, event: &core_types::OrderEvent, _ctx: &mut C) {
        if self.configured == 0 {
            return;
        }
        if event.kind == core_types::ORDER_EVENT_REJECTED {
            self.on_rejected(event.client_oid);
        }
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
            // PM15: Polymarket's rolling families ride the same channel
            // (venue byte Polymarket, `core_types::pm_roll`); this member's
            // families are Hyperliquid's, and a roll of another venue
            // names none of them — not even as a refusal to count.
            Some(core_types::ChannelId::InstrumentRoll)
                if event.venue == VenueId::Hyperliquid as u8 =>
            {
                self.on_roll(event)
            }
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
        // E5: the sweep now SENDS — LAW E-8's member half needs a ctx
        // to cancel through, where it used to only forget.
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
        /// E5: `(prev_client_oid, replacement)` per accepted modify.
        modifies: Vec<(u64, Order)>,
        /// E5: `client_oid` per accepted cancel.
        cancels: Vec<u64>,
        /// E5: when set, every lifecycle verb is refused with this —
        /// the `Unsupported` a ctx that never heard of them returns,
        /// or the `NoSuchOrder` a fill that won the race produces.
        lifecycle_err: Option<SubmitErr>,
    }
    impl Ctx for RecCtx {
        fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
            if self.full {
                return Err(SubmitErr::RingFull);
            }
            self.orders.push(order);
            Ok(())
        }
        fn cancel(&mut self, req: core_types::CancelReq) -> Result<(), SubmitErr> {
            if let Some(e) = self.lifecycle_err {
                return Err(e);
            }
            self.cancels.push(req.client_oid);
            Ok(())
        }
        fn modify(&mut self, prev_client_oid: u64, order: Order) -> Result<(), SubmitErr> {
            if let Some(e) = self.lifecycle_err {
                return Err(e);
            }
            self.modifies.push((prev_client_oid, order));
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
            modifies: Vec::new(),
            cancels: Vec::new(),
            lifecycle_err: None,
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
        bad.cap_day_usd_1e6 = 0;
        assert!(Bin15Strategy::new().configure(bad, luts(), anchor).is_err());
        let mut bad = params_1(FAMILY_OUT_15M);
        bad.requote_ttl_ns = 0;
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

    /// PM15: a Polymarket roll — the same channel, the same codec — never
    /// binds, settles or counts against a Hyperliquid family, whatever its
    /// family byte and strike say.
    #[test]
    fn a_polymarket_roll_is_not_this_members() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        m.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(900), false), &mut c);
        let mut pm = roll_event(0, 2650, 60, 79_000_000_000, expiry(900), false);
        pm.venue = VenueId::Polymarket as u8;
        m.on_venue_event(&pm, &mut c);
        let mut pm_settled = roll_event(0, 2649, 60, 0, expiry(900), true);
        pm_settled.venue = VenueId::Polymarket as u8;
        m.on_venue_event(&pm_settled, &mut c);
        assert_eq!(m.family(0).expect("f").live.outcome, 2649, "the HL instance stands");
        assert_eq!(m.counters().rolls, 1);
        assert_eq!(m.counters().rolls_settled, 0);
        assert_eq!(m.counters().spec_refused, 0, "not even counted");
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

    /// BIN15 P0 (F4). The book is FRESH and two-sided and the edge is
    /// 10 c wide — everything the taker needs — and the member still
    /// emits nothing, because the mark it would price off is nine
    /// seconds old. That is the whole point of the gate: the failure it
    /// catches looks exactly like a working member from every other
    /// counter.
    #[test]
    fn a_mark_the_tape_has_left_behind_prices_nothing() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        m.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(600), false), &mut c);
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), &mut c);
        let priced_before = m.counters().reprices;
        // 9 s after the last mark, against a 5 s shelf life.
        m.on_tick(&tick(no_sym(0), 400_000, 600_000, 1_000_000_000, at(70), false), &mut c);
        m.on_tick(&tick(yes_sym(0), 390_000, 400_000, 1_000_000_000, at(70), false), &mut c);
        assert_eq!(m.counters().skipped_mark_stale, 2, "one hold per leg tick");
        assert_eq!(
            m.counters().reprices,
            priced_before,
            "the pricer never ran on the dead mark"
        );
        assert!(c.orders.is_empty(), "nothing acts on a dead mark");
        // The very same book, once the mark catches up, IS priced — so the
        // gate is the mark's age and nothing else.
        m.on_venue_event(&mark_event(79_000_000_000, at(71)), &mut c);
        assert!(m.counters().reprices > priced_before, "a fresh mark re-prices");
        assert_eq!(m.counters().skipped_mark_stale, 2, "the counter does not move again");
    }

    /// The gate is a CONFIGURED shelf life, not a constant: an operator
    /// who widens it gets the old behaviour back, which is what makes
    /// the default a ruling rather than a hard-coded opinion.
    #[test]
    fn the_mark_shelf_life_is_the_operators_number() {
        let mut p = params_1(FAMILY_OUT_15M);
        p.mark_stale_ns = 30_000_000_000;
        let mut m = Bin15Strategy::new();
        m.configure(p, ramp_luts(), core_time::WallAnchor::new(MONO0, WALL0))
            .expect("configure");
        let mut c = ctx();
        warm(&mut m, 0);
        m.on_venue_event(&roll_event(0, 2649, 60, 79_000_000_000, expiry(600), false), &mut c);
        m.on_mark(0, 79_000_000_000, at(0));
        m.on_venue_event(&mark_event(79_000_000_000, at(61)), &mut c);
        m.on_tick(&tick(no_sym(0), 400_000, 600_000, 1_000_000_000, at(70), false), &mut c);
        m.on_tick(&tick(yes_sym(0), 390_000, 400_000, 1_000_000_000, at(70), false), &mut c);
        assert_eq!(m.counters().skipped_mark_stale, 0, "9 s is inside a 30 s shelf");
        assert_eq!(m.family(0).expect("f").p_ts_ns, at(70), "and the tick at 70 s priced");
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
        assert_ne!(m.family(0).expect("f").p_ts_ns, at(62) + 1, "VT4: stale is not evidence");
        // The prices ARE recorded — they are the last thing the venue
        // said — and the next FRESH tick prices on them.
        assert_eq!(m.family(0).expect("f").touch_yes.touch.ask_1e6, 400_000);
        m.on_tick(&tick(yes_sym(0), 390_000, 400_000, 1_000_000_000, at(63), false), &mut c);
        assert_eq!(m.family(0).expect("f").p_ts_ns, at(63), "the fresh tick priced");
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
        // τ = 600 s expiry − the 62 s mark; a family with no TWAP window
        // settles AT `T`, so its horizon is τ itself (BIN15 S3).
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

    /// BIN15 S3 (ruling O-2): inside the settlement window the member
    /// prices on the venue's RUNNING average — the part of `[T − W, T]`
    /// already behind, folded from its own marks with the last mark
    /// carried forward — and what it publishes is exactly
    /// `price::fair_value_twap` of the integral a reader redoes by hand.
    #[test]
    fn inside_the_window_the_member_prices_on_the_running_average() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        let k = 79_000_000_000i64;
        // Window [540 s, 600 s].
        m.on_venue_event(&roll_event(0, 2651, 60, k, expiry(600), false), &mut c);
        let above = k + k / 200; // +0.5 %
        let below = k - k / 1_000; // −0.1 %
        // A mark every 2 s from 530 s: one lands AT the open (in force
        // there), then 30 s of the window at +0.5 %.
        let mut s = 530u64;
        while s <= 568 {
            m.on_venue_event(&mark_event(above, at(s)), &mut c);
            s += 2;
        }
        // The mark dips under the strike at 570 s and is re-read at 571 s.
        m.on_venue_event(&mark_event(below, at(570)), &mut c);
        m.on_venue_event(&mark_event(below, at(571)), &mut c);

        let fam = m.family(0).expect("f");
        let a = i128::from(above) * 30_000_000_000 + i128::from(below) * 1_000_000_000;
        assert_eq!(fam.twap_sum, a, "∫ mark du over [540 s, 571 s], carried forward");
        assert_eq!(fam.twap_last_ts, expiry(571));
        assert_eq!(fam.twap_gap, 0, "a 2 s cadence is whole evidence");
        let sig2 = m.marks[0].sig2_min_1e18[FAMILY_OUT_15M as usize];
        let want = price::fair_value_twap(&m.lut, below, k, 29_000_000_000, 60_000_000_000, a, sig2)
            .expect("priced");
        assert_eq!(fam.p_ts_ns, at(571), "the last mark re-priced it");
        assert_eq!(fam.p_hat_1e6, want.p_hat_1e6);
        assert_eq!(fam.d_1e6, want.d_1e6);
        assert_eq!(fam.den_1e9, want.den_1e9);
        assert_eq!(fam.last_tau_ns, price::pricing_horizon_ns(29_000_000_000, 60_000_000_000));
        assert_eq!(fam.last_tau_ns, want.horizon_ns, "the member records the pricer's horizon");
        // The known half-minute above the strike outweighs the dip: the
        // same mark with nothing behind it is a belief BELOW one half.
        assert!(fam.d_1e6 > 0, "the average already behind decides: d {}", fam.d_1e6);
        let blind = price::fair_value(&m.lut, below, k, fam.last_tau_ns, sig2).expect("blind");
        assert!(blind.d_1e6 < 0, "without it the dip would read as a loss");
    }

    /// BIN15 S3: a hole in the member's own mark series across the window
    /// is a hole in the average. The harness refuses to settle on one
    /// (`SETTLE_MARK_GAP_MAX_NS`, the whole piece counted), and the member
    /// HOLDS on one — for the rest of the window, since the hole does not
    /// close — rather than price a number nobody saw. The successor starts
    /// clean.
    #[test]
    fn a_hole_in_the_window_holds_the_family_until_the_successor() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        let k = 79_000_000_000i64;
        m.on_venue_event(&roll_event(0, 2652, 60, k, expiry(600), false), &mut c);
        m.on_venue_event(&mark_event(k, at(530)), &mut c);
        let priced_at = m.family(0).expect("f").p_ts_ns;
        let held = m.counters().skipped_stale;
        // 530 s → 545 s: one 15 s piece across the open.
        m.on_venue_event(&mark_event(k, at(545)), &mut c);
        assert_eq!(m.family(0).expect("f").twap_gap, 1, "15 s outlasts the bound");
        assert!(
            m.marks[0].sig2_min_1e18[FAMILY_OUT_15M as usize] > 0,
            "the forecast is warm: every hold below is the window's"
        );
        let mut s = 547u64;
        while s <= 559 {
            m.on_venue_event(&mark_event(k, at(s)), &mut c);
            s += 2;
        }
        assert_eq!(m.counters().skipped_stale, held + 8, "every in-window reprice held");
        assert_eq!(m.family(0).expect("f").p_ts_ns, priced_at, "and none of them priced");

        // The successor is its own instance: its average starts empty.
        m.on_venue_event(&roll_event(0, 2652, 0, 0, 0, true), &mut c);
        m.on_venue_event(&roll_event(0, 2653, 60, k, expiry(1_500), false), &mut c);
        let fam = m.family(0).expect("f");
        assert_eq!((fam.twap_gap, fam.twap_sum, fam.twap_last_ts), (0, 0, 0));
    }

    /// BIN15 S3: an instance bound after its window opened — a boot or a
    /// late roll inside the minute — never saw the mark in force at the
    /// open, so the head of its average is unknown and it HOLDS there.
    #[test]
    fn an_instance_bound_inside_its_window_holds_there() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        warm(&mut m, 0);
        let k = 79_000_000_000i64;
        m.on_mark(0, k, at(0));
        m.on_venue_event(&mark_event(k, at(61)), &mut c);
        // Bound with its window [40 s, 100 s] already open at 61 s.
        m.on_venue_event(&roll_event(0, 2655, 60, k, expiry(100), false), &mut c);
        let priced_at = m.family(0).expect("f").p_ts_ns;
        let held = m.counters().skipped_stale;
        m.on_venue_event(&mark_event(k, at(63)), &mut c);
        let fam = m.family(0).expect("f");
        assert_eq!(fam.twap_gap, 1, "the open was never seen by this instance");
        assert!(
            m.marks[0].sig2_min_1e18[FAMILY_OUT_15M as usize] > 0,
            "the forecast is warm: the hold is the window's"
        );
        assert_eq!(m.counters().skipped_stale, held + 1);
        assert_eq!(fam.p_ts_ns, priced_at, "nothing priced on half an average");
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
        let before = m.counters().reprices;
        m.on_regime(
            RegimeGate::new(
                [core_types::RegimeWord(0); 4],
                false,
                core_types::REGIME_OFF_HARD,
            ),
            &mut c,
        );
        m.on_timer(at(70), &mut c);
        // BIN15 P0 (F4): the underlying keeps printing, so the gate
        // under test is the REGIME gate and nothing else.
        m.on_venue_event(&mark_event(79_000_000_000, at(70)), &mut c);
        m.on_tick(&tick(yes_sym(0), 390_000, 400_000, 1_000_000_000, at(71), false), &mut c);
        assert!(
            m.counters().reprices > before,
            "a 15-minute binary is not one of the regime lane's cells"
        );
    }

    // ---- BIN15 H90: the late-window entry ----------------------------

    /// A late Φ that is a CDF shape: 0.5 at z = 0 rising to 1.0 at z = 4.
    fn late_ramp() -> Box<late::LateLut> {
        let mut l = late::LateLut::flat();
        let mut i = 0usize;
        while i < late::LATE_PHI_POINTS {
            let z = i as i64 * late::LATE_PHI_STEP_1E6;
            l.phi[i] = (500_000 + z.min(4_000_000) / 8) as u32;
            i += 1;
        }
        Box::new(l)
    }

    /// A 15 m member with the late law ON at the ruled values.
    fn late_member(persist: u8) -> Bin15Strategy {
        let mut p = params_1(FAMILY_OUT_15M);
        p.late.enabled = 1;
        p.late.persist_polls = persist;
        let mut m = Bin15Strategy::new();
        m.configure(p, ramp_luts(), core_time::WallAnchor::new(MONO0, WALL0))
            .expect("configure");
        m.configure_late(late_ramp());
        m
    }

    /// Marks and perp BBOs once a second over `[from, to)` seconds after the
    /// anchor, around $79 000 ±$1, the perp a dollar wide around the mark.
    fn late_feed(m: &mut Bin15Strategy, c: &mut RecCtx, from: u64, to: u64) {
        let mut s = from;
        while s < to {
            let mark = 79_000_000_000 + if s % 2 == 0 { 1_000_000 } else { -1_000_000 };
            m.on_venue_event(&mark_event(mark, at(s) + 200_000_000), c);
            m.on_tick(
                &tick(btc_sym(), mark - 500_000, mark + 500_000, 1_000_000_000, at(s) + 300_000_000, false),
                c,
            );
            s += 1;
        }
    }

    /// One book poll: the Yes snapshot and its No mirror, at `secs + 0.4 s`.
    fn late_poll(m: &mut Bin15Strategy, c: &mut RecCtx, secs: u64, yes_bid: i64, yes_ask: i64) {
        let t = at(secs) + 400_000_000;
        m.on_tick(&tick(yes_sym(0), yes_bid, yes_ask, 1_000_000_000, t, false), c);
        m.on_tick(&tick(no_sym(0), ONE_1E6 - yes_ask, ONE_1E6 - yes_bid, 1_000_000_000, t, false), c);
    }

    /// The instance: struck $100 under the mark, expiring 600 s after the
    /// anchor — Yes is decided by the last half-minute.
    fn late_bind(m: &mut Bin15Strategy, c: &mut RecCtx) {
        m.on_venue_event(&roll_event(0, 7001, 60, 78_900_000_000, expiry(600), false), c);
    }

    #[test]
    fn the_late_entry_buys_the_decided_side_after_two_polls_at_the_whole_ask_capped() {
        let mut m = late_member(2);
        let mut c = ctx();
        late_bind(&mut m, &mut c);
        late_feed(&mut m, &mut c, 0, 570);
        assert_eq!(m.counters().late_reprices, 0, "nothing priced before the window");
        // Inside the window with no book yet: priced, and held on the book.
        late_feed(&mut m, &mut c, 570, 580);
        assert_eq!(m.counters().late_reprices, 10);
        assert!(m.counters().late_skipped_book > 0, "no ask to buy");
        // First poll at τ ≈ 19.6 s: the test holds, the run is one.
        late_poll(&mut m, &mut c, 580, 800_000, 850_000);
        assert!(c.orders.is_empty(), "one poll is not two");
        assert!(m.counters().late_skipped_persist > 0);
        // Marks between polls are not polls.
        late_feed(&mut m, &mut c, 581, 585);
        assert!(c.orders.is_empty(), "a mark never extends the run");
        // Second poll: it fires — one IoC bid on Yes at the ask, $100 / 0.85
        // = 117 whole contracts (the resting size is larger).
        late_poll(&mut m, &mut c, 585, 800_000, 850_000);
        assert_eq!(c.orders.len(), 1, "exactly one late entry");
        let o = c.orders[0];
        assert_eq!(o.sym, yes_sym(0));
        assert_eq!(o.side, Side::Bid);
        assert_eq!(o.px.raw(), 850_000);
        assert_eq!(o.qty.raw(), 117_000_000);
        assert_eq!(m.counters().late_submitted, 1);
        let st = *m.late_state(0).expect("family 0");
        assert_eq!(st.covered, 1);
        assert_eq!(st.oid, o.client_oid);
        assert!(st.entry_p_1e6 >= 980_000, "a decided belief: {}", st.entry_p_1e6);
        assert!(st.ff_ts != 0 && st.ff_yes == 1, "the P1b counterfactual is the first poll");
        // Once per instance: further polls emit nothing.
        late_feed(&mut m, &mut c, 586, 590);
        late_poll(&mut m, &mut c, 590, 800_000, 850_000);
        assert_eq!(c.orders.len(), 1, "one late entry per instance");
    }

    #[test]
    fn the_late_law_waits_for_its_window_and_holds_the_tail_the_s_law_refuses() {
        let mut m = late_member(1);
        let mut c = ctx();
        late_bind(&mut m, &mut c);
        late_feed(&mut m, &mut c, 0, 560);
        // τ ≈ 39.6 s: outside [2, 30) — nothing; and a book the late test
        // refuses (0.99 is not 2 c under a decided belief) until the tail.
        late_poll(&mut m, &mut c, 560, 980_000, 990_000);
        assert!(c.orders.is_empty());
        assert_eq!(m.counters().late_reprices, 0);
        // τ ≈ 5.6 s: inside the S-law's 10 s tail, inside the late window —
        // the late law prices and (persist 1) fires on the cheaper ask.
        late_feed(&mut m, &mut c, 560, 594);
        late_poll(&mut m, &mut c, 594, 800_000, 850_000);
        assert_eq!(c.orders.len(), 1, "the late window is its own floor");
        assert!(m.counters().skipped_tail > 0, "the S-law still held there");
    }

    #[test]
    fn an_ask_that_does_not_clear_the_margin_or_a_cold_grid_holds() {
        // The ask at 0.99 against a belief ~1.0: under the 2 c margin? 1.0 −
        // 0.02 = 0.98 < 0.99, so refused on price.
        let mut m = late_member(1);
        let mut c = ctx();
        late_bind(&mut m, &mut c);
        late_feed(&mut m, &mut c, 0, 585);
        late_poll(&mut m, &mut c, 585, 980_000, 990_000);
        assert!(c.orders.is_empty());
        assert!(m.counters().late_skipped_price > 0);
        // A grid with under 300 returns holds (σ).
        let mut m = late_member(1);
        let mut c = ctx();
        late_bind(&mut m, &mut c);
        late_feed(&mut m, &mut c, 400, 585);
        late_poll(&mut m, &mut c, 585, 800_000, 850_000);
        assert!(c.orders.is_empty());
        assert!(m.counters().late_skipped_stale > 0);
    }

    #[test]
    fn a_refused_late_entry_retries_inside_its_window_and_a_roll_resets_it() {
        let mut m = late_member(1);
        let mut c = ctx();
        late_bind(&mut m, &mut c);
        late_feed(&mut m, &mut c, 0, 585);
        late_poll(&mut m, &mut c, 585, 800_000, 850_000);
        assert_eq!(c.orders.len(), 1);
        let oid = c.orders[0].client_oid;
        // The venue refuses it late (a gateway's REJECTED).
        let mut ev = core_types::OrderEvent::ZERO;
        ev.client_oid = oid;
        ev.kind = core_types::ORDER_EVENT_REJECTED;
        ev.ts_ns = at(586);
        m.on_order_event(&ev, &mut c);
        assert_eq!(m.late_state(0).expect("f").covered, 0, "the entry may retry");
        late_feed(&mut m, &mut c, 586, 588);
        late_poll(&mut m, &mut c, 588, 800_000, 850_000);
        assert_eq!(c.orders.len(), 2, "it retried");
        // The successor instance starts clean.
        m.on_venue_event(&roll_event(0, 7002, 60, 79_000_000_000, expiry(1_500), false), &mut c);
        assert_eq!(*m.late_state(0).expect("f"), LateState::default());
    }

    /// One late entry on the decided Yes side at 0.85 (persist 1); its oid.
    fn late_entry(m: &mut Bin15Strategy, c: &mut RecCtx) -> u64 {
        late_bind(m, c);
        late_feed(m, c, 0, 585);
        late_poll(m, c, 585, 800_000, 850_000);
        assert_eq!(c.orders.len(), 1, "one late entry");
        c.orders[0].client_oid
    }

    #[test]
    fn a_late_take_moves_the_position_only_on_its_fills_and_books_every_print() {
        let mut m = late_member(1);
        let mut c = ctx();
        let oid = late_entry(&mut m, &mut c);
        let q = c.orders[0].qty.raw();
        assert_eq!(m.family(0).expect("f").pos_yes_1e6, 0, "a submit is an intent");
        let fill = |qty: i64, oid: u64| {
            Fill::new(at(586), yes_sym(0), Side::Bid, Price::from_raw(850_000), Qty::from_raw(qty), oid)
        };
        // Two prints of one IoC: a partial stays in flight, the second closes it.
        m.on_fill(&fill(17_000_000, oid), &mut c);
        assert_eq!(m.family(0).expect("f").pend_take.filled_1e6, 17_000_000);
        assert!(m.family(0).expect("f").pend_take.live(), "a partial is still in flight");
        m.on_fill(&fill(q - 17_000_000, oid), &mut c);
        assert_eq!(m.family(0).expect("f").pos_yes_1e6, q, "both prints booked");
        assert!(!m.family(0).expect("f").pend_take.live(), "full: closed");
        let k = m.counters();
        assert_eq!((k.fills, k.takes_filled, k.late_filled, k.unknown_fills), (2, 1, 1, 0));
        // A fill for an order this member never sent changes nothing.
        m.on_fill(&fill(1_000_000, 0xDEAD), &mut c);
        assert_eq!(m.family(0).expect("f").pos_yes_1e6, q);
        assert_eq!(m.counters().unknown_fills, 1);
        // The venue's settlement print is counted as what it is, never as
        // a disagreement with the dispatcher, and closes nothing twice.
        let settle = Fill::new(at(601), yes_sym(0), Side::Ask, Price::from_raw(1_000_000), Qty::from_raw(q), 0xDEAD_BEEF)
            .with_flags(core_types::FILL_FLAG_SETTLEMENT);
        m.on_fill(&settle, &mut c);
        assert_eq!((m.counters().settlement_fills, m.counters().unknown_fills), (1, 1));
        assert_eq!(m.family(0).expect("f").pos_yes_1e6, q);
    }

    #[test]
    fn an_unfilled_late_take_is_written_off_at_its_deadline_and_gives_its_room_back() {
        let mut m = late_member(1);
        let mut c = ctx();
        let _ = late_entry(&mut m, &mut c);
        assert!(m.family(0).expect("f").notional_instance_1e6 > 0, "the take reserved its room");
        m.on_timer(at(585) + 400_000_000, &mut c);
        assert!(m.family(0).expect("f").pend_take.live(), "before its deadline: pending");
        assert_eq!(m.counters().takes_unfilled, 0);
        m.on_timer(at(590), &mut c);
        assert!(!m.family(0).expect("f").pend_take.live(), "after it: written off");
        assert_eq!(m.counters().takes_unfilled, 1);
        assert_eq!(m.family(0).expect("f").notional_instance_1e6, 0, "the room came back");
    }

    #[test]
    fn the_oid_carries_family_side_and_outcome() {
        let mut m = late_member(1);
        let mut c = ctx();
        let oid = late_entry(&mut m, &mut c);
        assert_eq!(oid & OID_OUTCOME_MASK, 7001, "outcome in bits 0..32");
        assert_eq!((oid >> OID_FAMILY_SHIFT) & 0xFFFF, 0, "family in 32..48");
        assert_eq!((oid >> OID_SIDE_SHIFT) & 1, 0, "Yes leg");
        assert_eq!(oid >> 63, 0, "bit 63, the retired null-arm flag, is always 0");
        assert_ne!(oid, 0, "and an oid is never zero, which is what marks a slot free");
    }

    #[test]
    fn the_late_take_is_capped_by_the_instance_and_the_day() {
        let configured = |instance: i64, day: i64| {
            let mut p = params_1(FAMILY_OUT_15M);
            p.late.enabled = 1;
            p.late.persist_polls = 1;
            p.late.usd_1e6 = instance;
            p.cap_instance_usd_1e6 = instance;
            p.cap_day_usd_1e6 = day;
            let mut m = Bin15Strategy::new();
            m.configure(p, ramp_luts(), core_time::WallAnchor::new(MONO0, WALL0))
                .expect("configure");
            m.configure_late(late_ramp());
            m
        };
        // $10 at 0.85 is 11 whole contracts ($9.35).
        let mut m = configured(10_000_000, 100_000_000);
        let mut c = ctx();
        let _ = late_entry(&mut m, &mut c);
        assert_eq!(c.orders[0].qty.raw(), 11_000_000);
        // A day with under one contract of room: counted, nothing sent.
        let mut m = configured(10_000_000, 500_000);
        let mut c = ctx();
        late_bind(&mut m, &mut c);
        late_feed(&mut m, &mut c, 0, 585);
        late_poll(&mut m, &mut c, 585, 800_000, 850_000);
        assert!(c.orders.is_empty());
        assert!(m.counters().late_skipped_cap > 0);
    }

    #[test]
    fn a_full_order_ring_is_counted_and_books_nothing() {
        let mut m = late_member(1);
        let mut c = ctx();
        c.full = true;
        late_bind(&mut m, &mut c);
        late_feed(&mut m, &mut c, 0, 585);
        late_poll(&mut m, &mut c, 585, 800_000, 850_000);
        assert!(m.orders_dropped() >= 1, "each try is refused by the ring");
        assert_eq!(m.orders_emitted(), 0);
        assert_eq!(m.counters().takes_submitted, 0, "a dropped order is not a submit");
        assert!(!m.family(0).expect("f").pend_take.live(), "and books no pending");
        assert_eq!(m.family(0).expect("f").notional_instance_1e6, 0, "nor any notional");
        assert_eq!(m.late_state(0).expect("f").covered, 0, "so the entry may retry");
    }

    #[test]
    fn with_the_late_law_off_nothing_is_priced_or_fed() {
        let mut m = member(FAMILY_OUT_15M);
        let mut c = ctx();
        late_bind(&mut m, &mut c);
        late_feed(&mut m, &mut c, 0, 585);
        late_poll(&mut m, &mut c, 585, 800_000, 850_000);
        assert!(c.orders.is_empty());
        let k = m.counters();
        assert_eq!(
            (k.late_reprices, k.late_submitted, k.late_skipped_stale),
            (0, 0, 0),
            "off is the member as it was"
        );
    }

    #[test]
    fn on_start_refuses_a_late_law_without_its_table() {
        let mut p = params_1(FAMILY_OUT_15M);
        p.late.enabled = 1;
        let mut m = Bin15Strategy::new();
        m.configure(p, ramp_luts(), core_time::WallAnchor::new(MONO0, WALL0))
            .expect("configure");
        let mut c = ctx();
        assert!(m.on_start(&mut c).is_err(), "no table, no start");
        m.configure_late(late_ramp());
        assert!(m.on_start(&mut c).is_ok());
        // And configure holds core-config's bounds.
        let mut bad = params_1(FAMILY_OUT_15M);
        bad.late.tau_max_ns = 61_000_000_000;
        assert!(Bin15Strategy::new()
            .configure(bad, ramp_luts(), core_time::WallAnchor::new(MONO0, WALL0))
            .is_err());
    }
}
