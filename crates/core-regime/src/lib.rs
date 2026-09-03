// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-regime — the measured market regime (RG1)
//!
//! Owner doc: `docs/regime-and-dashboard-plan.md` §3–§4. This crate is
//! the engine's half of operator decision D1 ("measured in the engine,
//! declared by the AI"): it keeps one minute-close ring per member
//! symbol, re-judges every dimension of every horizon profile once per
//! wall minute with an INTEGER law that the Python reference
//! (`claude_worker.regime`) reproduces bit for bit, applies hysteresis,
//! and resolves the EFFECTIVE word per profile from the declared word
//! (fresh under its TTL) over the measured one.
//!
//! ## Hot-path cost (doctrine §2.10)
//!
//! * `on_tick`: one linear-probe lookup (non-members reject in ~1
//!   probe at 25 % load), one compare against the precomputed minute
//!   boundary, one store. No division, no allocation, no float.
//! * `on_timer`: nothing until a minute boundary; then one ring write
//!   per member and a judge pass — a few thousand integer ops for both
//!   profiles (RV over 240 minute returns, ER over 48 five-minute
//!   steps, one return per member). Direct recompute is deliberate:
//!   it is cheap enough that running sums would only add state to keep
//!   in parity.
//! * Everything else (`configure`, `seed`) is boot-only and may do
//!   arbitrary work; nothing here allocates after `new_boxed`.
//!
//! ## Minute law
//!
//! A minute's close is the LAST fresh mid seen with `ts_ns` inside the
//! minute (`[end − 60 s, end)` in monotonic time, boundaries from the
//! boot [`WallAnchor`]). A tick that lands after the boundary but before
//! the roll is parked as the next minute's first sample. A minute with
//! no fresh tick is a hole (`0`); readers look back at most
//! [`MAX_BACK_MIN`] minutes for a close and treat longer gaps as ABSENT.
//! Stale ticks (`TICK_FLAG_STALE`) never enter the ring.
//!
//! ## Judgement law (per profile, on the just-closed minute `m`)
//!
//! | dim | law |
//! |---|---|
//! | TREND | `r = ret_W(btc)`; `up/dn` = members with `ret_W > +thr` / `< −thr` over the present members `n`; BULL ⇔ `r > thr ∧ up·1e9 ≥ q·n`, BEAR symmetric, else NEUTRAL; fewer than half the members present ⇒ ABSENT |
//! | SHAPE | `ER = |c(m) − c(m−W)| · 1e9 / Σ_j |c(m−5j) − c(m−5j−5)|` (5-minute steps, ≥ 80 % present; flat window ⇒ ER 0); bands `lo_enter/lo_exit/hi_enter/hi_exit` decide CHOP / MIXED / TREND relative to the COMMITTED state |
//! | VOL | `RV = isqrt(Σ r_k²)`, `r_k` = 1-minute bps×1e9 returns inside W (≥ 80 % present); `< p30` LOW, `> p70` HIGH, else NORMAL; both percentiles 0 ⇒ ABSENT |
//! | FUND_SIGN | latest funding print of the funding ref: `< 0` NEG else POS; no print ⇒ ABSENT |
//! | FUND_LEVEL | the same print vs `p30/p70` (absolute rates ×1e9); both 0 ⇒ ABSENT |
//! | STRETCH | `ret_W · 1e9 / RV`: `> +k` EXT_UP, `< −k` EXT_DOWN, else NEUTRAL; RV 0 ⇒ ABSENT |
//! | REL (per member) | `ret_W(member) − ret_W(btc)`: `< −thr` LAGGING, `> +thr` LEADING, else INLINE |
//!
//! Every dimension then passes the CONFIRM law: a new value (ABSENT
//! included) must repeat `confirm_min` consecutive minutes before it
//! commits. A committed ABSENT is the per-dimension unknown mark in the
//! measured word (`core_types::regime::DIM_UNKNOWN_BIT`).
//!
//! ## Effective law
//!
//! `effective = merge(declared over measured) | DECLARED` while the
//! declaration is fresh (`now − declared_ts < ttl`) — a declaration
//! overrides only the dimensions it names, so a partial declaration is
//! a per-dimension override; else `measured | MEASURED` when at least
//! one dimension is known; else `RegimeWord::UNKNOWN`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod math;

use core_time::{NsTs, WallAnchor};
use core_types::regime::{
    DIM_SHAPE, DIM_SOURCE, DIM_UNKNOWN_BIT, FUND_NEG, FUND_POS, LEVEL_HIGH, LEVEL_LOW,
    LEVEL_NORMAL, REGIME_PROFILES, REL_INLINE, REL_LAGGING, REL_LEADING, REL_UNKNOWN, SHAPE_CHOP,
    SHAPE_MIXED, SHAPE_TREND, SOURCE_DECLARED, SOURCE_MEASURED, STRETCH_EXT_DOWN, STRETCH_EXT_UP,
    STRETCH_NEUTRAL, TREND_BEAR, TREND_BULL, TREND_NEUTRAL, VOL_HIGH, VOL_LOW, VOL_NORMAL,
};
use core_types::{RegimeWord, SymbolId, Tick, SYMBOL_ID_NONE, TICK_FLAG_STALE};

use crate::math::{floor_div, isqrt_i128, ret_bps_1e9};

/// Members of the breadth set (the BTC reference takes the 32nd slot).
pub const REGIME_MAX_MEMBERS: usize = 31;
/// Symbol slots: BTC reference + members.
pub const REGIME_MAX_SYMS: usize = REGIME_MAX_MEMBERS + 1;
/// Minute closes kept per symbol (25.6 h — the 4 h `slow` profile with
/// headroom for a 24 h profile without a layout change).
pub const REGIME_RING_MIN: usize = 1536;
/// Longest window a profile may configure (one less than the ring so
/// `m − W` is always inside it).
pub const REGIME_WINDOW_MAX_MIN: u16 = (REGIME_RING_MIN - 1) as u16;
/// Shortest window (one ER step).
pub const REGIME_WINDOW_MIN_MIN: u16 = 5;
/// How far back a close lookup walks over holes before ABSENT.
pub const MAX_BACK_MIN: u16 = 5;
/// Slot in the symbol → slot map meaning "not a member".
pub const SLOT_NONE: u8 = 0xFF;
/// The BTC reference's slot.
pub const SLOT_BTC: u8 = 0;
/// Nanoseconds per wall minute.
pub const MINUTE_NS: u64 = 60_000_000_000;
/// The "no value" candidate/committed judgement (⇒ unknown mark).
pub const ABSENT: u8 = 0xFF;

const MAP_SLOTS: usize = 128;
const SCALE_1E9: i128 = 1_000_000_000;
const DIM_ER_STEP_MIN: u16 = 5;

/// One horizon profile's parameters — `regime.toml` `[profile.<name>]`
/// (plan §4.6). Integers only; ×1e9 unless the name says bps or min.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct ProfileParams {
    /// TREND return window, minutes.
    pub trend_w_min: u16,
    /// SHAPE (efficiency-ratio) window, minutes — a multiple of 5.
    pub shape_w_min: u16,
    /// VOL (realized-vol) window, minutes.
    pub vol_w_min: u16,
    /// STRETCH window, minutes (return AND RV over this window).
    pub stretch_w_min: u16,
    /// REL window, minutes.
    pub rel_w_min: u16,
    /// FUND_LEVEL history length in prints (worker-side lookback; the
    /// engine carries it so the struct mirrors the file).
    pub fund_prints: u16,
    /// Explicit padding — always zero.
    _pad0: u32,
    /// TREND: BTC return beyond ±thr counts as directional (bps ×1e9).
    pub trend_thr_bps_1e9: i64,
    /// TREND: share of present members that must agree (×1e9).
    pub breadth_q_1e9: i64,
    /// SHAPE: enter CHOP below (×1e9).
    pub er_lo_enter_1e9: i64,
    /// SHAPE: leave CHOP at/above (×1e9).
    pub er_lo_exit_1e9: i64,
    /// SHAPE: enter TREND above (×1e9).
    pub er_hi_enter_1e9: i64,
    /// SHAPE: leave TREND at/below (×1e9).
    pub er_hi_exit_1e9: i64,
    /// VOL: p30 of the lookback (bps ×1e9); 0 with p70 0 = ABSENT.
    pub rv_p30_bps_1e9: i64,
    /// VOL: p70 of the lookback (bps ×1e9).
    pub rv_p70_bps_1e9: i64,
    /// STRETCH: `|ret / RV| > k` ⇒ extended (×1e9).
    pub stretch_k_1e9: i64,
    /// REL: coin vs BTC beyond ±thr ⇒ lagging/leading (bps ×1e9).
    pub rel_thr_bps_1e9: i64,
    /// FUND_LEVEL: p30 of the print history (rate ×1e9); 0 with p70 0 = ABSENT.
    pub fund_p30_1e9: i64,
    /// FUND_LEVEL: p70 of the print history (rate ×1e9).
    pub fund_p70_1e9: i64,
}

impl ProfileParams {
    /// Construct without naming the padding.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        trend_w_min: u16,
        shape_w_min: u16,
        vol_w_min: u16,
        stretch_w_min: u16,
        rel_w_min: u16,
        fund_prints: u16,
        trend_thr_bps_1e9: i64,
        breadth_q_1e9: i64,
        er_lo_enter_1e9: i64,
        er_lo_exit_1e9: i64,
        er_hi_enter_1e9: i64,
        er_hi_exit_1e9: i64,
        rv_p30_bps_1e9: i64,
        rv_p70_bps_1e9: i64,
        stretch_k_1e9: i64,
        rel_thr_bps_1e9: i64,
        fund_p30_1e9: i64,
        fund_p70_1e9: i64,
    ) -> Self {
        Self {
            trend_w_min,
            shape_w_min,
            vol_w_min,
            stretch_w_min,
            rel_w_min,
            fund_prints,
            _pad0: 0,
            trend_thr_bps_1e9,
            breadth_q_1e9,
            er_lo_enter_1e9,
            er_lo_exit_1e9,
            er_hi_enter_1e9,
            er_hi_exit_1e9,
            rv_p30_bps_1e9,
            rv_p70_bps_1e9,
            stretch_k_1e9,
            rel_thr_bps_1e9,
            fund_p30_1e9,
            fund_p70_1e9,
        }
    }

    /// The plan §3.2 / `regime.toml.example` defaults for `fast` (1 h).
    pub const FAST_DEFAULT: Self = Self::new(
        60,
        60,
        60,
        60,
        60,
        9,
        30_000_000_000,
        600_000_000,
        300_000_000,
        350_000_000,
        600_000_000,
        550_000_000,
        0,
        0,
        2_000_000_000,
        50_000_000_000,
        0,
        0,
    );

    /// The plan §3.2 / `regime.toml.example` defaults for `slow` (4 h).
    pub const SLOW_DEFAULT: Self = Self::new(
        240,
        240,
        240,
        240,
        240,
        90,
        80_000_000_000,
        600_000_000,
        300_000_000,
        350_000_000,
        600_000_000,
        550_000_000,
        0,
        0,
        2_000_000_000,
        150_000_000_000,
        0,
        0,
    );

    /// Boot validation (`configure` refuses on any `Err`).
    #[allow(clippy::manual_range_contains)] // const fn: RangeInclusive::contains is not const
    pub const fn validate(&self) -> Result<(), RegimeErr> {
        let wins = [
            self.trend_w_min,
            self.shape_w_min,
            self.vol_w_min,
            self.stretch_w_min,
            self.rel_w_min,
        ];
        let mut i = 0;
        while i < wins.len() {
            if wins[i] < REGIME_WINDOW_MIN_MIN || wins[i] > REGIME_WINDOW_MAX_MIN {
                return Err(RegimeErr::Window);
            }
            i += 1;
        }
        if self.shape_w_min % DIM_ER_STEP_MIN != 0 {
            return Err(RegimeErr::Window);
        }
        if self.trend_thr_bps_1e9 <= 0
            || self.stretch_k_1e9 <= 0
            || self.rel_thr_bps_1e9 <= 0
            || self.breadth_q_1e9 <= 0
            || self.breadth_q_1e9 > SCALE_1E9 as i64
        {
            return Err(RegimeErr::Threshold);
        }
        if !(self.er_lo_enter_1e9 < self.er_lo_exit_1e9
            && self.er_lo_exit_1e9 <= self.er_hi_exit_1e9
            && self.er_hi_exit_1e9 < self.er_hi_enter_1e9
            && self.er_lo_enter_1e9 >= 0
            && self.er_hi_enter_1e9 <= SCALE_1E9 as i64)
        {
            return Err(RegimeErr::Bands);
        }
        if self.rv_p30_bps_1e9 < 0
            || self.rv_p70_bps_1e9 < self.rv_p30_bps_1e9
            || self.fund_p70_1e9 < self.fund_p30_1e9
        {
            return Err(RegimeErr::Percentile);
        }
        Ok(())
    }
}

/// The detector's boot parameters — `regime.toml` resolved against the
/// live universe (descriptors → `SymbolId`s happen in the cli, the
/// `icdp.toml` way).
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct RegimeParams {
    /// TREND / SHAPE / VOL / STRETCH price reference.
    pub btc_ref: SymbolId,
    /// FUND_SIGN / FUND_LEVEL reference (its Funding events).
    pub fund_ref: SymbolId,
    /// Breadth-set members (`members[..n_members]`), never the ref.
    pub members: [SymbolId; REGIME_MAX_MEMBERS],
    /// Live member count.
    pub n_members: u8,
    /// Consecutive agreeing minutes before a dimension flips (≥ 1).
    pub confirm_min: u8,
    /// Explicit padding — always zero.
    _pad0: [u8; 6],
    /// Per-profile parameters (index = profile).
    pub profiles: [ProfileParams; REGIME_PROFILES],
}

impl RegimeParams {
    /// Construct without naming the padding (`members` beyond
    /// `n_members` are ignored).
    pub const fn new(
        btc_ref: SymbolId,
        fund_ref: SymbolId,
        members: [SymbolId; REGIME_MAX_MEMBERS],
        n_members: u8,
        confirm_min: u8,
        profiles: [ProfileParams; REGIME_PROFILES],
    ) -> Self {
        Self {
            btc_ref,
            fund_ref,
            members,
            n_members,
            confirm_min,
            _pad0: [0; 6],
            profiles,
        }
    }

    /// Boot validation: real refs, ≤ 31 unique members none of which is
    /// the BTC ref, `confirm_min ≥ 1`, every profile valid.
    pub const fn validate(&self) -> Result<(), RegimeErr> {
        if self.btc_ref == SYMBOL_ID_NONE || self.fund_ref == SYMBOL_ID_NONE {
            return Err(RegimeErr::Ref);
        }
        if self.n_members as usize > REGIME_MAX_MEMBERS {
            return Err(RegimeErr::Members);
        }
        let mut i = 0usize;
        while i < self.n_members as usize {
            let s = self.members[i];
            if s == SYMBOL_ID_NONE || s == self.btc_ref {
                return Err(RegimeErr::Members);
            }
            let mut j = 0usize;
            while j < i {
                if self.members[j] == s {
                    return Err(RegimeErr::Members);
                }
                j += 1;
            }
            i += 1;
        }
        if self.confirm_min == 0 {
            return Err(RegimeErr::Confirm);
        }
        let mut p = 0usize;
        while p < REGIME_PROFILES {
            if let Err(e) = self.profiles[p].validate() {
                return Err(e);
            }
            p += 1;
        }
        Ok(())
    }
}

/// Why `configure` refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegimeErr {
    /// A reference symbol is `SYMBOL_ID_NONE`.
    Ref,
    /// Too many, duplicate, none, or ref-equal members.
    Members,
    /// A window outside `[5, 1535]` or a SHAPE window not a multiple of 5.
    Window,
    /// A non-positive threshold, or `breadth_q` outside `(0, 1]`.
    Threshold,
    /// ER bands not ordered `lo_enter < lo_exit ≤ hi_exit < hi_enter` inside `[0, 1]`.
    Bands,
    /// `p70 < p30` (VOL or funding), or a negative RV percentile.
    Percentile,
    /// `confirm_min == 0`.
    Confirm,
}

/// One seed row (boot): a member's minute close from `candles.db`
/// (`claude_worker.regime --seed-out`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct SeedRow {
    /// Member or reference symbol.
    pub sym: SymbolId,
    /// Explicit padding — always zero.
    _pad0: u32,
    /// Wall minute index (`epoch_seconds / 60`).
    pub minute: i64,
    /// Minute close, mid ×1e6 (`> 0`).
    pub close_1e6: i64,
}

impl SeedRow {
    /// Construct without naming the padding.
    pub const fn new(sym: SymbolId, minute: i64, close_1e6: i64) -> Self {
        Self {
            sym,
            _pad0: 0,
            minute,
            close_1e6,
        }
    }
}

/// The raw numbers behind one profile's judgement (metrics + `/state`).
/// `present` bit i ⇒ field i is meaningful: 0 ret, 1 er, 2 rv, 3
/// stretch, 4 funding, 5 breadth.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct RegimeRaw {
    /// BTC return over the TREND window, bps ×1e9.
    pub ret_bps_1e9: i64,
    /// Efficiency ratio over the SHAPE window, ×1e9.
    pub er_1e9: i64,
    /// Realized vol over the VOL window, bps ×1e9.
    pub rv_bps_1e9: i64,
    /// `ret / RV` over the STRETCH window, ×1e9.
    pub stretch_1e9: i64,
    /// Latest funding print of the funding ref, rate ×1e9.
    pub funding_1e9: i64,
    /// Members with `ret_W > +thr`.
    pub breadth_up: u8,
    /// Members with `ret_W < −thr`.
    pub breadth_dn: u8,
    /// Members with a present return.
    pub breadth_n: u8,
    /// Presence bits (see the struct docs).
    pub present: u8,
    /// Explicit padding — always zero.
    _pad0: [u8; 4],
}

/// `RegimeRaw::present` bits.
pub const RAW_RET: u8 = 1 << 0;
/// See [`RAW_RET`].
pub const RAW_ER: u8 = 1 << 1;
/// See [`RAW_RET`].
pub const RAW_RV: u8 = 1 << 2;
/// See [`RAW_RET`].
pub const RAW_STRETCH: u8 = 1 << 3;
/// See [`RAW_RET`].
pub const RAW_FUNDING: u8 = 1 << 4;
/// See [`RAW_RET`].
pub const RAW_BREADTH: u8 = 1 << 5;

/// Per-dimension confirm state (committed value + pending candidate).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
struct Judge {
    cur: u8,
    pending: u8,
    pend_n: u8,
    _pad: u8,
}

impl Judge {
    const EMPTY: Self = Self {
        cur: ABSENT,
        pending: ABSENT,
        pend_n: 0,
        _pad: 0,
    };

    /// The confirm law. Returns `true` when the committed value changed
    /// between two KNOWN values (a flip).
    #[inline]
    fn feed(&mut self, candidate: u8, confirm_min: u8) -> bool {
        if candidate == self.cur {
            self.pending = candidate;
            self.pend_n = 0;
            return false;
        }
        if candidate == self.pending {
            self.pend_n = self.pend_n.saturating_add(1);
        } else {
            self.pending = candidate;
            self.pend_n = 1;
        }
        if self.pend_n >= confirm_min {
            let flip = self.cur != ABSENT && candidate != ABSENT;
            self.cur = candidate;
            self.pend_n = 0;
            return flip;
        }
        false
    }
}

/// One member's minute-close ring + the two minute latches.
#[repr(C, align(64))]
struct RegimeSym {
    ring: [i64; REGIME_RING_MIN],
    sym: SymbolId,
    cur_set: u8,
    next_set: u8,
    _pad0: [u8; 2],
    cur_mid_1e6: i64,
    next_mid_1e6: i64,
    next_ts: NsTs,
}

/// The detector. Boot-boxed (`new_boxed`), configured once, then fed
/// ticks / funding / the 1 s timer. See the crate docs for the laws.
#[repr(C, align(64))]
pub struct RegimeState {
    syms: [RegimeSym; REGIME_MAX_SYMS],
    map_sym: [SymbolId; MAP_SLOTS],
    map_slot: [u8; MAP_SLOTS],
    params: RegimeParams,
    anchor: WallAnchor,
    /// The minute currently accumulating (wall minute index).
    minute: i64,
    /// Monotonic end of `minute` (exclusive).
    minute_end_mono: NsTs,
    n_syms: u8,
    configured: u8,
    _pad0: [u8; 6],
    judges: [[Judge; 8]; REGIME_PROFILES],
    rel_judges: [[Judge; REGIME_MAX_SYMS]; REGIME_PROFILES],
    measured: [RegimeWord; 4],
    declared: [RegimeWord; 4],
    effective: [RegimeWord; 4],
    declared_ts: [NsTs; 4],
    declared_ttl: [u64; 4],
    raw: [RegimeRaw; REGIME_PROFILES],
    funding_rate_1e9: i64,
    funding_ts_ms: u64,
    flips: [[u64; 8]; REGIME_PROFILES],
    disagree: [u64; REGIME_PROFILES],
    minutes_judged: u64,
    seed_rows: u32,
    _pad1: u32,
}

impl RegimeState {
    /// Heap-direct zeroed construction, then the sentinel fills the
    /// all-zero pattern cannot express — boot only.
    pub fn new_boxed() -> Box<RegimeState> {
        let layout = core::alloc::Layout::new::<RegimeState>();
        // SAFETY: RegimeState is a `#[repr(C)]` POD of integers and
        // fixed arrays; the all-zero pattern is a valid (unconfigured)
        // value, and every sentinel that is not zero is written right
        // below before the box is handed out. alloc_zeroed returns
        // memory valid for the layout; Box::from_raw takes sole
        // ownership. A null return aborts at boot (fail-fast).
        let mut b = unsafe {
            let p = std::alloc::alloc_zeroed(layout).cast::<RegimeState>();
            assert!(!p.is_null(), "RegimeState boot allocation failed");
            Box::from_raw(p)
        };
        b.reset_sentinels();
        b
    }

    fn reset_sentinels(&mut self) {
        let mut i = 0;
        while i < MAP_SLOTS {
            self.map_sym[i] = SYMBOL_ID_NONE;
            self.map_slot[i] = SLOT_NONE;
            i += 1;
        }
        let mut s = 0;
        while s < REGIME_MAX_SYMS {
            self.syms[s].sym = SYMBOL_ID_NONE;
            s += 1;
        }
        let mut p = 0;
        while p < REGIME_PROFILES {
            self.judges[p] = [Judge::EMPTY; 8];
            self.rel_judges[p] = [Judge::EMPTY; REGIME_MAX_SYMS];
            p += 1;
        }
        let mut w = 0;
        while w < 4 {
            self.measured[w] = RegimeWord::UNKNOWN;
            self.declared[w] = RegimeWord::EMPTY;
            self.effective[w] = RegimeWord::UNKNOWN;
            w += 1;
        }
    }

    // -----------------------------------------------------------
    // Boot
    // -----------------------------------------------------------

    /// Validate + install the parameters, bind the symbol map, and
    /// anchor the minute clock at `now`. Refuses (state untouched) on
    /// any validation error.
    pub fn configure(
        &mut self,
        params: &RegimeParams,
        anchor: WallAnchor,
        now: NsTs,
    ) -> Result<(), RegimeErr> {
        params.validate()?;
        self.reset_sentinels();
        self.params = *params;
        self.anchor = anchor;
        self.syms[SLOT_BTC as usize].sym = params.btc_ref;
        self.map_insert(params.btc_ref, SLOT_BTC);
        let mut i = 0usize;
        while i < params.n_members as usize {
            let slot = (i + 1) as u8;
            self.syms[slot as usize].sym = params.members[i];
            self.map_insert(params.members[i], slot);
            i += 1;
        }
        self.n_syms = params.n_members + 1;
        let wall_ns = anchor.wall_of(now);
        self.minute = (wall_ns / MINUTE_NS) as i64;
        self.minute_end_mono = anchor.mono_of((self.minute as u64 + 1) * MINUTE_NS);
        self.configured = 1;
        self.minutes_judged = 0;
        self.seed_rows = 0;
        Ok(())
    }

    /// True after a successful `configure`.
    #[inline(always)]
    pub const fn is_configured(&self) -> bool {
        self.configured != 0
    }

    /// Boot seed: minute closes for `[minute − RING, minute)` from the
    /// worker's `candles.db` export. Rows outside the ring, for
    /// non-members, non-positive, or at/after the current minute are
    /// skipped. After filling, the last `2·confirm_min` closed minutes
    /// are re-judged in order so the confirm law starts warm (flips
    /// during this replay are not counted). Returns rows applied.
    pub fn seed(&mut self, rows: &[SeedRow]) -> u32 {
        if !self.is_configured() {
            return 0;
        }
        let lo = self.minute - REGIME_RING_MIN as i64;
        let mut applied = 0u32;
        let mut i = 0usize;
        while i < rows.len() {
            let r = rows[i];
            i += 1;
            if r.close_1e6 <= 0 || r.minute >= self.minute || r.minute <= lo {
                continue;
            }
            let slot = self.lookup(r.sym);
            if slot == SLOT_NONE {
                continue;
            }
            self.syms[slot as usize].ring[ring_idx(r.minute)] = r.close_1e6;
            applied += 1;
        }
        self.seed_rows = self.seed_rows.saturating_add(applied);
        if applied > 0 {
            let replay = 2 * self.params.confirm_min as i64;
            let now = self.minute_end_mono.wrapping_sub(MINUTE_NS);
            let mut m = self.minute - replay;
            while m < self.minute {
                self.judge_minute(m, false, now);
                m += 1;
            }
        }
        applied
    }

    // -----------------------------------------------------------
    // Hot path
    // -----------------------------------------------------------

    #[inline(always)]
    fn map_hash(sym: SymbolId) -> usize {
        ((sym ^ (sym >> 24)).wrapping_mul(0x9E37_79B1) >> 25) as usize & (MAP_SLOTS - 1)
    }

    fn map_insert(&mut self, sym: SymbolId, slot: u8) {
        let mut h = Self::map_hash(sym);
        let mut n = 0;
        while self.map_sym[h] != SYMBOL_ID_NONE {
            debug_assert!(self.map_sym[h] != sym, "duplicate member");
            h = (h + 1) & (MAP_SLOTS - 1);
            n += 1;
            debug_assert!(n < MAP_SLOTS, "map full");
        }
        self.map_sym[h] = sym;
        self.map_slot[h] = slot;
    }

    /// Symbol → slot, `SLOT_NONE` for non-members. Linear probe; the
    /// empty sentinel terminates every miss.
    #[inline(always)]
    pub fn lookup(&self, sym: SymbolId) -> u8 {
        let mut h = Self::map_hash(sym);
        loop {
            // SAFETY: `h` is masked to `MAP_SLOTS − 1` on every step.
            let s = unsafe { *self.map_sym.get_unchecked(h) };
            if s == sym {
                // SAFETY: same index law.
                return unsafe { *self.map_slot.get_unchecked(h) };
            }
            if s == SYMBOL_ID_NONE {
                return SLOT_NONE;
            }
            h = (h + 1) & (MAP_SLOTS - 1);
        }
    }

    /// One fresh tick: park its mid as the current minute's latest
    /// sample (or the next minute's first, past the boundary). Stale
    /// ticks, one-sided books and non-members cost one branch each.
    #[inline(always)]
    pub fn on_tick(&mut self, t: &Tick) {
        if t.flags & TICK_FLAG_STALE != 0 {
            return;
        }
        let slot = self.lookup(t.sym);
        if slot == SLOT_NONE {
            return;
        }
        let bid = t.bid_px.raw();
        let ask = t.ask_px.raw();
        if bid <= 0 || ask <= 0 {
            return;
        }
        let mid = (bid + ask) >> 1;
        // SAFETY: `slot < n_syms ≤ REGIME_MAX_SYMS` — the map only
        // ever holds slots written by `configure`.
        let s = unsafe { self.syms.get_unchecked_mut(slot as usize) };
        if t.ts_ns < self.minute_end_mono {
            s.cur_mid_1e6 = mid;
            s.cur_set = 1;
        } else {
            s.next_mid_1e6 = mid;
            s.next_set = 1;
            s.next_ts = t.ts_ns;
        }
    }

    /// One funding print of the FUNDING reference (the caller filters
    /// the event by `sym == fund_ref`): rate ×1e9, venue time ms.
    #[inline(always)]
    pub fn on_funding(&mut self, rate_1e9: i64, venue_time_ms: u64) {
        self.funding_rate_1e9 = rate_1e9;
        self.funding_ts_ms = if venue_time_ms == 0 { 1 } else { venue_time_ms };
    }

    /// The 1 s timer: roll every completed minute (judging each), then
    /// refresh the effective words. Returns a bit per profile whose
    /// EFFECTIVE word changed — the set fans `on_regime` out on it.
    #[inline]
    pub fn on_timer(&mut self, now: NsTs) -> u8 {
        if !self.is_configured() {
            return 0;
        }
        while now >= self.minute_end_mono {
            self.roll(now);
        }
        self.refresh_effective(now)
    }

    fn roll(&mut self, now: NsTs) {
        let idx = ring_idx(self.minute);
        let new_end = self.minute_end_mono + MINUTE_NS;
        let mut s = 0usize;
        while s < self.n_syms as usize {
            let sym = &mut self.syms[s];
            sym.ring[idx] = if sym.cur_set != 0 { sym.cur_mid_1e6 } else { 0 };
            if sym.next_set != 0 && sym.next_ts < new_end {
                sym.cur_mid_1e6 = sym.next_mid_1e6;
                sym.cur_set = 1;
                sym.next_set = 0;
            } else {
                sym.cur_set = 0;
            }
            s += 1;
        }
        let closed = self.minute;
        self.minute += 1;
        self.minute_end_mono = new_end;
        self.judge_minute(closed, true, now);
    }

    // -----------------------------------------------------------
    // Declared / effective
    // -----------------------------------------------------------

    /// Install a declaration for profile `p` (shape-checked upstream:
    /// SOURCE empty, `ttl_ns > 0`). Out-of-range profiles are ignored.
    #[inline]
    pub fn set_declared(&mut self, p: u8, word: RegimeWord, now: NsTs, ttl_ns: u64) {
        if p as usize >= REGIME_PROFILES {
            return;
        }
        self.declared[p as usize] = word;
        self.declared_ts[p as usize] = now;
        self.declared_ttl[p as usize] = ttl_ns;
    }

    /// Drop profile `p`'s declaration (expire-on-silence, operator clear).
    #[inline]
    pub fn clear_declared(&mut self, p: u8) {
        if p as usize >= REGIME_PROFILES {
            return;
        }
        self.declared[p as usize] = RegimeWord::EMPTY;
        self.declared_ts[p as usize] = 0;
        self.declared_ttl[p as usize] = 0;
    }

    /// True while profile `p`'s declaration is inside its TTL.
    #[inline(always)]
    pub fn declared_fresh(&self, p: u8, now: NsTs) -> bool {
        let i = p as usize;
        i < REGIME_PROFILES
            && self.declared_ttl[i] != 0
            && now.wrapping_sub(self.declared_ts[i]) < self.declared_ttl[i]
    }

    /// Re-resolve every profile's effective word; returns the changed
    /// bits (bit = profile).
    pub fn refresh_effective(&mut self, now: NsTs) -> u8 {
        let mut changed = 0u8;
        let mut p = 0usize;
        while p < REGIME_PROFILES {
            let eff = self.compute_effective(p, now);
            if eff != self.effective[p] {
                self.effective[p] = eff;
                changed |= 1 << p;
            }
            p += 1;
        }
        changed
    }

    fn compute_effective(&self, p: usize, now: NsTs) -> RegimeWord {
        let m = self.measured[p];
        if self.declared_fresh(p as u8, now) {
            return merge_declared(self.declared[p], m);
        }
        if any_known(m) {
            m.with_source(SOURCE_MEASURED)
        } else {
            RegimeWord::UNKNOWN
        }
    }

    // -----------------------------------------------------------
    // Judgement
    // -----------------------------------------------------------

    /// Judge profile-by-profile on the just-closed minute `m`; `now` is
    /// the roll instant (the freshness reference for the disagree law —
    /// the same instant the effective law sees).
    fn judge_minute(&mut self, m: i64, count: bool, now: NsTs) {
        let confirm = self.params.confirm_min;
        let n_members = self.params.n_members as usize;
        let mut p = 0usize;
        while p < REGIME_PROFILES {
            let pp = self.params.profiles[p];
            let btc = &self.syms[SLOT_BTC as usize].ring;
            let mut raw = RegimeRaw::default();

            // --- BTC-only dimensions ---
            let ret = ret_over(btc, m, pp.trend_w_min);
            if let Some(r) = ret {
                raw.ret_bps_1e9 = r;
                raw.present |= RAW_RET;
            }
            let er = er_over(btc, m, pp.shape_w_min);
            if let Some(e) = er {
                raw.er_1e9 = e;
                raw.present |= RAW_ER;
            }
            let rv = rv_over(btc, m, pp.vol_w_min);
            if let Some(v) = rv {
                raw.rv_bps_1e9 = v;
                raw.present |= RAW_RV;
            }
            let stretch = match (
                ret_over(btc, m, pp.stretch_w_min),
                rv_over(btc, m, pp.stretch_w_min),
            ) {
                (Some(r), Some(v)) if v > 0 => {
                    let s = floor_div(r as i128 * SCALE_1E9, v as i128) as i64;
                    raw.stretch_1e9 = s;
                    raw.present |= RAW_STRETCH;
                    Some(s)
                }
                _ => None,
            };

            // --- breadth + REL over the members ---
            let mut up = 0u8;
            let mut dn = 0u8;
            let mut present = 0u8;
            let mut i = 0usize;
            while i < n_members {
                let slot = i + 1;
                let member = &self.syms[slot].ring;
                let rel_cand = match (
                    ret_over(member, m, pp.rel_w_min),
                    ret_over(btc, m, pp.rel_w_min),
                ) {
                    (Some(a), Some(b)) => judge_rel(a - b, pp.rel_thr_bps_1e9),
                    _ => ABSENT,
                };
                self.rel_judges[p][slot].feed(rel_cand, confirm);
                if let Some(r) = ret_over(member, m, pp.trend_w_min) {
                    present += 1;
                    if r > pp.trend_thr_bps_1e9 {
                        up += 1;
                    } else if r < -pp.trend_thr_bps_1e9 {
                        dn += 1;
                    }
                }
                i += 1;
            }
            raw.breadth_up = up;
            raw.breadth_dn = dn;
            raw.breadth_n = present;
            let breadth_ok = n_members == 0 || (present as usize) * 2 >= n_members;
            if breadth_ok && n_members > 0 {
                raw.present |= RAW_BREADTH;
            }

            // --- funding ---
            let funding = if self.funding_ts_ms != 0 {
                raw.funding_1e9 = self.funding_rate_1e9;
                raw.present |= RAW_FUNDING;
                Some(self.funding_rate_1e9)
            } else {
                None
            };

            // --- candidates ---
            let trend_cand = match ret {
                Some(r) if breadth_ok => judge_trend(r, up, dn, present, n_members, &pp),
                _ => ABSENT,
            };
            let shape_cand = match er {
                Some(e) => judge_shape(self.judges[p][DIM_SHAPE as usize].cur, e, &pp),
                None => ABSENT,
            };
            let vol_cand = match rv {
                Some(v) => judge_vol(v, &pp),
                None => ABSENT,
            };
            let fund_cand = match funding {
                Some(f) => judge_fund_sign(f),
                None => ABSENT,
            };
            let level_cand = match funding {
                Some(f) => judge_fund_level(f, &pp),
                None => ABSENT,
            };
            let stretch_cand = match stretch {
                Some(s) => judge_stretch(s, &pp),
                None => ABSENT,
            };

            let cands = [
                trend_cand,
                shape_cand,
                vol_cand,
                fund_cand,
                level_cand,
                stretch_cand,
            ];
            let mut d = 0usize;
            while d < DIM_SOURCE as usize {
                if self.judges[p][d].feed(cands[d], confirm) && count {
                    self.flips[p][d] += 1;
                }
                d += 1;
            }

            // --- the measured word ---
            let mut w = 0u64;
            let mut d = 0u8;
            while d < DIM_SOURCE {
                let cur = self.judges[p][d as usize].cur;
                let byte = if cur == ABSENT {
                    DIM_UNKNOWN_BIT
                } else {
                    1u8 << cur
                };
                w |= (byte as u64) << (8 * d as u32);
                d += 1;
            }
            let measured = RegimeWord(w).with_source(SOURCE_MEASURED);
            if count
                && self.declared_fresh(p as u8, now)
                && declared_disagrees(self.declared[p], measured)
            {
                self.disagree[p] += 1;
            }
            self.measured[p] = measured;
            self.raw[p] = raw;
            p += 1;
        }
        if count {
            self.minutes_judged += 1;
        }
    }

    // -----------------------------------------------------------
    // Readers
    // -----------------------------------------------------------

    /// The effective word of profile `p` (UNKNOWN for out-of-range).
    #[inline(always)]
    pub fn effective(&self, p: u8) -> RegimeWord {
        if (p as usize) < REGIME_PROFILES {
            self.effective[p as usize]
        } else {
            RegimeWord::UNKNOWN
        }
    }

    /// The measured word of profile `p`.
    #[inline(always)]
    pub fn measured(&self, p: u8) -> RegimeWord {
        if (p as usize) < REGIME_PROFILES {
            self.measured[p as usize]
        } else {
            RegimeWord::UNKNOWN
        }
    }

    /// The declared word of profile `p` (EMPTY when none / cleared).
    #[inline(always)]
    pub fn declared(&self, p: u8) -> RegimeWord {
        if (p as usize) < REGIME_PROFILES {
            self.declared[p as usize]
        } else {
            RegimeWord::EMPTY
        }
    }

    /// Engine-monotonic stamp of profile `p`'s declaration (0 = none).
    #[inline]
    pub fn declared_ts(&self, p: u8) -> NsTs {
        let i = p as usize;
        if i < REGIME_PROFILES && self.declared_ttl[i] != 0 {
            self.declared_ts[i]
        } else {
            0
        }
    }

    /// TTL of profile `p`'s declaration (0 = none).
    #[inline]
    pub fn declared_ttl(&self, p: u8) -> u64 {
        let i = p as usize;
        if i < REGIME_PROFILES {
            self.declared_ttl[i]
        } else {
            0
        }
    }

    /// Age of profile `p`'s declaration, or `u64::MAX` when none.
    #[inline]
    pub fn declared_age_ns(&self, p: u8, now: NsTs) -> u64 {
        let i = p as usize;
        if i < REGIME_PROFILES && self.declared_ttl[i] != 0 {
            now.wrapping_sub(self.declared_ts[i])
        } else {
            u64::MAX
        }
    }

    /// The committed REL value of `sym` on profile `p`
    /// (`REL_UNKNOWN` for non-members, the BTC ref, or warm-up).
    #[inline(always)]
    pub fn rel_of(&self, p: u8, sym: SymbolId) -> u8 {
        let slot = self.lookup(sym);
        self.rel_of_slot(p, slot)
    }

    /// [`Self::rel_of`] by slot (the VM caches slots per row).
    #[inline(always)]
    pub fn rel_of_slot(&self, p: u8, slot: u8) -> u8 {
        if (p as usize) >= REGIME_PROFILES || slot == SLOT_NONE || slot == SLOT_BTC {
            return REL_UNKNOWN;
        }
        let cur = self.rel_judges[p as usize][slot as usize].cur;
        if cur == ABSENT {
            REL_UNKNOWN
        } else {
            cur
        }
    }

    /// The raw judgement inputs of profile `p`.
    #[inline]
    pub fn raw(&self, p: u8) -> RegimeRaw {
        if (p as usize) < REGIME_PROFILES {
            self.raw[p as usize]
        } else {
            RegimeRaw::default()
        }
    }

    /// Flip counter of dimension `d` on profile `p`.
    #[inline]
    pub fn flips(&self, p: u8, d: u8) -> u64 {
        if (p as usize) < REGIME_PROFILES && d < 8 {
            self.flips[p as usize][d as usize]
        } else {
            0
        }
    }

    /// Minutes on which a fresh declaration disagreed with the
    /// measurement on a dimension it named.
    #[inline]
    pub fn disagree(&self, p: u8) -> u64 {
        if (p as usize) < REGIME_PROFILES {
            self.disagree[p as usize]
        } else {
            0
        }
    }

    /// Minutes judged since configure (seed replay excluded).
    #[inline]
    pub const fn minutes_judged(&self) -> u64 {
        self.minutes_judged
    }

    /// Seed rows applied.
    #[inline]
    pub const fn seed_rows(&self) -> u32 {
        self.seed_rows
    }

    /// The minute currently accumulating.
    #[inline]
    pub const fn minute(&self) -> i64 {
        self.minute
    }

    /// Monotonic end (exclusive) of the accumulating minute.
    #[inline]
    pub const fn minute_end_mono(&self) -> NsTs {
        self.minute_end_mono
    }

    /// Member count including the BTC ref.
    #[inline]
    pub const fn n_syms(&self) -> u8 {
        self.n_syms
    }

    /// The installed parameters.
    #[inline]
    pub const fn params(&self) -> &RegimeParams {
        &self.params
    }

    /// The latest funding print `(rate_1e9, venue_time_ms)`; ms 0 = none.
    #[inline]
    pub const fn funding(&self) -> (i64, u64) {
        (self.funding_rate_1e9, self.funding_ts_ms)
    }

    /// A member's minute close (0 = hole), for tests and the harness.
    pub fn close(&self, slot: u8, minute: i64) -> i64 {
        if slot == SLOT_NONE || slot as usize >= self.n_syms as usize {
            return 0;
        }
        self.syms[slot as usize].ring[ring_idx(minute)]
    }
}

// ---------------------------------------------------------------
// The pure law (mirrored in claude_worker.regime)
// ---------------------------------------------------------------

#[inline(always)]
fn ring_idx(minute: i64) -> usize {
    minute.rem_euclid(REGIME_RING_MIN as i64) as usize
}

/// The close at minute `m`, walking back over holes at most
/// [`MAX_BACK_MIN`] minutes; 0 = absent.
#[inline]
pub fn close_at(ring: &[i64; REGIME_RING_MIN], m: i64) -> i64 {
    let mut k = 0i64;
    while k <= MAX_BACK_MIN as i64 {
        let c = ring[ring_idx(m - k)];
        if c > 0 {
            return c;
        }
        k += 1;
    }
    0
}

/// `ret_bps_1e9(close(m − w), close(m))`, `None` when either is absent.
#[inline]
pub fn ret_over(ring: &[i64; REGIME_RING_MIN], m: i64, w: u16) -> Option<i64> {
    let from = close_at(ring, m - w as i64);
    let to = close_at(ring, m);
    if from <= 0 || to <= 0 {
        return None;
    }
    Some(ret_bps_1e9(from, to))
}

/// Efficiency ratio ×1e9 over `w` minutes in 5-minute steps (≥ 80 %
/// of the steps present, else `None`; a flat window is 0).
#[inline]
pub fn er_over(ring: &[i64; REGIME_RING_MIN], m: i64, w: u16) -> Option<i64> {
    let steps = (w / DIM_ER_STEP_MIN) as i64;
    if steps == 0 {
        return None;
    }
    let mut den: i128 = 0;
    let mut present = 0i64;
    let mut j = 0i64;
    while j < steps {
        let a = close_at(ring, m - 5 * j);
        let b = close_at(ring, m - 5 * j - 5);
        if a > 0 && b > 0 {
            den += (a as i128 - b as i128).abs();
            present += 1;
        }
        j += 1;
    }
    if present * 5 < steps * 4 {
        return None;
    }
    let first = close_at(ring, m - w as i64);
    let last = close_at(ring, m);
    if first <= 0 || last <= 0 {
        return None;
    }
    if den == 0 {
        return Some(0);
    }
    let num = (last as i128 - first as i128).abs();
    Some(floor_div(num * SCALE_1E9, den) as i64)
}

/// Realized vol in bps ×1e9 over `w` one-minute returns (≥ 80 %
/// present, else `None`).
#[inline]
pub fn rv_over(ring: &[i64; REGIME_RING_MIN], m: i64, w: u16) -> Option<i64> {
    let mut sum: i128 = 0;
    let mut present = 0i64;
    let mut k = 0i64;
    while k < w as i64 {
        let a = close_at(ring, m - k - 1);
        let b = close_at(ring, m - k);
        if a > 0 && b > 0 {
            let r = ret_bps_1e9(a, b) as i128;
            sum += r * r;
            present += 1;
        }
        k += 1;
    }
    if present * 5 < (w as i64) * 4 {
        return None;
    }
    Some(isqrt_i128(sum))
}

/// TREND candidate from the BTC return + breadth counts.
#[inline]
pub fn judge_trend(
    r: i64,
    up: u8,
    dn: u8,
    present: u8,
    n_members: usize,
    pp: &ProfileParams,
) -> u8 {
    let agree_up =
        n_members == 0 || (up as i128) * SCALE_1E9 >= pp.breadth_q_1e9 as i128 * present as i128;
    let agree_dn =
        n_members == 0 || (dn as i128) * SCALE_1E9 >= pp.breadth_q_1e9 as i128 * present as i128;
    if r > pp.trend_thr_bps_1e9 && agree_up {
        TREND_BULL
    } else if r < -pp.trend_thr_bps_1e9 && agree_dn {
        TREND_BEAR
    } else {
        TREND_NEUTRAL
    }
}

/// SHAPE candidate: the band machine relative to the committed state.
#[inline]
pub fn judge_shape(cur: u8, er: i64, pp: &ProfileParams) -> u8 {
    if cur == SHAPE_CHOP {
        if er < pp.er_lo_exit_1e9 {
            SHAPE_CHOP
        } else if er > pp.er_hi_enter_1e9 {
            SHAPE_TREND
        } else {
            SHAPE_MIXED
        }
    } else if cur == SHAPE_TREND {
        if er > pp.er_hi_exit_1e9 {
            SHAPE_TREND
        } else if er < pp.er_lo_enter_1e9 {
            SHAPE_CHOP
        } else {
            SHAPE_MIXED
        }
    } else if er < pp.er_lo_enter_1e9 {
        SHAPE_CHOP
    } else if er > pp.er_hi_enter_1e9 {
        SHAPE_TREND
    } else {
        SHAPE_MIXED
    }
}

/// VOL candidate (`ABSENT` while both percentiles are 0).
#[inline]
pub fn judge_vol(rv: i64, pp: &ProfileParams) -> u8 {
    if pp.rv_p30_bps_1e9 == 0 && pp.rv_p70_bps_1e9 == 0 {
        ABSENT
    } else if rv < pp.rv_p30_bps_1e9 {
        VOL_LOW
    } else if rv > pp.rv_p70_bps_1e9 {
        VOL_HIGH
    } else {
        VOL_NORMAL
    }
}

/// FUND_SIGN candidate.
#[inline]
pub fn judge_fund_sign(rate_1e9: i64) -> u8 {
    if rate_1e9 < 0 {
        FUND_NEG
    } else {
        FUND_POS
    }
}

/// FUND_LEVEL candidate (`ABSENT` while both percentiles are 0).
#[inline]
pub fn judge_fund_level(rate_1e9: i64, pp: &ProfileParams) -> u8 {
    if pp.fund_p30_1e9 == 0 && pp.fund_p70_1e9 == 0 {
        ABSENT
    } else if rate_1e9 < pp.fund_p30_1e9 {
        LEVEL_LOW
    } else if rate_1e9 > pp.fund_p70_1e9 {
        LEVEL_HIGH
    } else {
        LEVEL_NORMAL
    }
}

/// STRETCH candidate.
#[inline]
pub fn judge_stretch(stretch_1e9: i64, pp: &ProfileParams) -> u8 {
    if stretch_1e9 > pp.stretch_k_1e9 {
        STRETCH_EXT_UP
    } else if stretch_1e9 < -pp.stretch_k_1e9 {
        STRETCH_EXT_DOWN
    } else {
        STRETCH_NEUTRAL
    }
}

/// REL candidate from `ret(member) − ret(btc)`.
#[inline]
pub fn judge_rel(rel_bps_1e9: i64, thr: i64) -> u8 {
    if rel_bps_1e9 < -thr {
        REL_LAGGING
    } else if rel_bps_1e9 > thr {
        REL_LEADING
    } else {
        REL_INLINE
    }
}

/// True when at least one market dimension of `w` holds a known value.
#[inline]
pub fn any_known(w: RegimeWord) -> bool {
    let mut d = 0u8;
    while d < DIM_SOURCE {
        let b = w.dim(d);
        if b != 0 && b != DIM_UNKNOWN_BIT {
            return true;
        }
        d += 1;
    }
    false
}

/// The effective word under a fresh declaration: every dimension the
/// declaration names (including an explicit unknown mark) replaces the
/// measured byte; the rest stay measured; SOURCE = DECLARED.
#[inline]
pub fn merge_declared(declared: RegimeWord, measured: RegimeWord) -> RegimeWord {
    let mut w = 0u64;
    let mut d = 0u8;
    while d < DIM_SOURCE {
        let db = declared.dim(d);
        let byte = if db != 0 { db } else { measured.dim(d) };
        w |= (byte as u64) << (8 * d as u32);
        d += 1;
    }
    RegimeWord(w).with_source(SOURCE_DECLARED)
}

/// True when the declaration names a dimension whose measured byte
/// differs (the `engine_regime_disagree_total` law).
#[inline]
pub fn declared_disagrees(declared: RegimeWord, measured: RegimeWord) -> bool {
    let mut d = 0u8;
    while d < DIM_SOURCE {
        let db = declared.dim(d);
        if db != 0 && db != measured.dim(d) {
            return true;
        }
        d += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::regime::{
        DIM_FUND_LEVEL, DIM_FUND_SIGN, DIM_STRETCH, DIM_TREND, DIM_VOL, SOURCE_UNKNOWN,
    };
    use core_types::{make_symbol_id, Price, Qty, VenueId};
    use proptest::prelude::*;

    const BTC: SymbolId = make_symbol_id(VenueId::Binance, 100);
    const ETH: SymbolId = make_symbol_id(VenueId::Binance, 101);
    const SOL: SymbolId = make_symbol_id(VenueId::Binance, 102);
    const XRP: SymbolId = make_symbol_id(VenueId::Binance, 103);
    const OTHER: SymbolId = make_symbol_id(VenueId::Okx, 7);

    /// Anchor: mono 1e12 == wall 1_800_000_000 s (a minute boundary).
    const T0_MONO: NsTs = 1_000_000_000_000;
    const T0_WALL: u64 = 1_800_000_000 * 1_000_000_000;
    const M0: i64 = (T0_WALL / MINUTE_NS) as i64;

    fn anchor() -> WallAnchor {
        WallAnchor::new(T0_MONO, T0_WALL)
    }

    fn short_profile() -> ProfileParams {
        // Windows of 10 minutes so tests warm fast; vol percentiles set.
        ProfileParams::new(
            10,
            10,
            10,
            10,
            10,
            9,
            30_000_000_000,
            600_000_000,
            300_000_000,
            350_000_000,
            600_000_000,
            550_000_000,
            10_000_000_000,
            100_000_000_000,
            2_000_000_000,
            50_000_000_000,
            -100_000,
            100_000,
        )
    }

    fn params(n_members: u8, confirm: u8) -> RegimeParams {
        let mut members = [SYMBOL_ID_NONE; REGIME_MAX_MEMBERS];
        members[0] = ETH;
        members[1] = SOL;
        members[2] = XRP;
        RegimeParams::new(
            BTC,
            BTC,
            members,
            n_members,
            confirm,
            [short_profile(); REGIME_PROFILES],
        )
    }

    fn state(n_members: u8, confirm: u8) -> Box<RegimeState> {
        let mut s = RegimeState::new_boxed();
        s.configure(&params(n_members, confirm), anchor(), T0_MONO)
            .expect("valid params");
        s
    }

    fn tick(sym: SymbolId, ts: NsTs, mid_1e6: i64) -> Tick {
        Tick::new(
            ts,
            VenueId::Binance,
            sym,
            1,
            Price(mid_1e6 - 500),
            Qty(1_000_000),
            Price(mid_1e6 + 500),
            Qty(1_000_000),
        )
    }

    fn stale_tick(sym: SymbolId, ts: NsTs, mid_1e6: i64) -> Tick {
        Tick::new_stamped(
            ts,
            VenueId::Binance,
            sym,
            1,
            Price(mid_1e6 - 500),
            Qty(1_000_000),
            Price(mid_1e6 + 500),
            Qty(1_000_000),
            0,
            TICK_FLAG_STALE,
        )
    }

    /// Feed `n` minutes of closes for a set of symbols through the
    /// tick + timer path, continuing from the state's current minute;
    /// `price(sym_index, k)` gives the mid of the k-th fed minute.
    fn run_minutes(
        s: &mut RegimeState,
        syms: &[SymbolId],
        n: i64,
        price: impl Fn(usize, i64) -> i64,
    ) -> u8 {
        let mut changed = 0u8;
        let mut k = 0i64;
        while k < n {
            let end = s.minute_end_mono();
            let ts = end - 30_000_000_000; // mid-minute
            for (i, sym) in syms.iter().enumerate() {
                s.on_tick(&tick(*sym, ts, price(i, k)));
            }
            // The timer fires just after the minute boundary.
            changed |= s.on_timer(end + 1_000_000);
            k += 1;
        }
        changed
    }

    #[test]
    fn configure_validates_and_binds_the_map() {
        let s = state(3, 3);
        assert!(s.is_configured());
        assert_eq!(s.n_syms(), 4);
        assert_eq!(s.lookup(BTC), SLOT_BTC);
        assert_eq!(s.lookup(ETH), 1);
        assert_eq!(s.lookup(XRP), 3);
        assert_eq!(s.lookup(OTHER), SLOT_NONE);
        assert_eq!(s.lookup(SYMBOL_ID_NONE), SLOT_NONE);
        assert_eq!(s.minute(), M0);
        assert_eq!(s.effective(0), RegimeWord::UNKNOWN);
        assert_eq!(s.effective(9), RegimeWord::UNKNOWN);

        let mut bad = params(3, 3);
        bad.confirm_min = 0;
        assert_eq!(bad.validate(), Err(RegimeErr::Confirm));
        let mut bad = params(3, 3);
        bad.members[1] = BTC;
        assert_eq!(bad.validate(), Err(RegimeErr::Members));
        let mut bad = params(3, 3);
        bad.members[2] = ETH;
        assert_eq!(bad.validate(), Err(RegimeErr::Members));
        let mut bad = params(3, 3);
        bad.btc_ref = SYMBOL_ID_NONE;
        assert_eq!(bad.validate(), Err(RegimeErr::Ref));
        let mut bad = params(3, 3);
        bad.profiles[0].shape_w_min = 12;
        assert_eq!(bad.validate(), Err(RegimeErr::Window));
        let mut bad = params(3, 3);
        bad.profiles[1].trend_w_min = REGIME_WINDOW_MAX_MIN + 1;
        assert_eq!(bad.validate(), Err(RegimeErr::Window));
        let mut bad = params(3, 3);
        bad.profiles[0].er_hi_exit_1e9 = bad.profiles[0].er_hi_enter_1e9;
        assert_eq!(bad.validate(), Err(RegimeErr::Bands));
        let mut bad = params(3, 3);
        bad.profiles[0].rv_p70_bps_1e9 = 1;
        assert_eq!(bad.validate(), Err(RegimeErr::Percentile));
        let mut bad = params(3, 3);
        bad.profiles[0].breadth_q_1e9 = 2_000_000_000;
        assert_eq!(bad.validate(), Err(RegimeErr::Threshold));
        // A refused configure leaves the state unconfigured.
        let mut s2 = RegimeState::new_boxed();
        assert_eq!(
            s2.configure(&bad, anchor(), T0_MONO),
            Err(RegimeErr::Threshold)
        );
        assert!(!s2.is_configured());
        assert_eq!(s2.on_timer(T0_MONO + MINUTE_NS), 0);
        assert_eq!(ProfileParams::FAST_DEFAULT.validate(), Ok(()));
        assert_eq!(ProfileParams::SLOW_DEFAULT.validate(), Ok(()));
    }

    #[test]
    fn minute_law_last_fresh_mid_closes_the_minute_and_late_ticks_park() {
        let mut s = state(0, 1);
        let m = M0;
        s.on_tick(&tick(BTC, T0_MONO + 1, 100_000_000));
        s.on_tick(&tick(BTC, T0_MONO + 30_000_000_000, 101_000_000));
        s.on_tick(&stale_tick(BTC, T0_MONO + 40_000_000_000, 999_000_000)); // ignored
        s.on_tick(&tick(OTHER, T0_MONO + 41_000_000_000, 5_000_000)); // non-member
                                                                      // A tick past the boundary before the roll parks as next.
        s.on_tick(&tick(BTC, T0_MONO + MINUTE_NS + 5, 102_000_000));
        assert_eq!(s.on_timer(T0_MONO + MINUTE_NS - 1), 0); // not yet
        assert_eq!(s.close(SLOT_BTC, m), 0);
        s.on_timer(T0_MONO + MINUTE_NS + 10);
        assert_eq!(s.minute(), m + 1);
        assert_eq!(s.close(SLOT_BTC, m), 101_000_000);
        // The parked tick became minute m+1's sample; no further tick.
        s.on_timer(T0_MONO + 2 * MINUTE_NS + 10);
        assert_eq!(s.close(SLOT_BTC, m + 1), 102_000_000);
        // A minute with no fresh tick is a hole.
        s.on_timer(T0_MONO + 3 * MINUTE_NS + 10);
        assert_eq!(s.close(SLOT_BTC, m + 2), 0);
        // A stalled timer rolls every skipped minute.
        s.on_timer(T0_MONO + 7 * MINUTE_NS + 10);
        assert_eq!(s.minute(), m + 7);
        // One-sided books never enter.
        let mut t = tick(BTC, T0_MONO + 7 * MINUTE_NS + 5, 100_000_000);
        t.ask_px = Price(0);
        s.on_tick(&t);
        s.on_timer(T0_MONO + 8 * MINUTE_NS + 10);
        assert_eq!(s.close(SLOT_BTC, m + 7), 0);
    }

    #[test]
    fn close_at_walks_back_over_holes_up_to_the_limit() {
        let mut ring = [0i64; REGIME_RING_MIN];
        ring[ring_idx(100)] = 5;
        assert_eq!(close_at(&ring, 100), 5);
        assert_eq!(close_at(&ring, 105), 5);
        assert_eq!(close_at(&ring, 106), 0);
        assert_eq!(close_at(&ring, 99), 0);
        assert_eq!(ret_over(&ring, 105, 10), None);
        ring[ring_idx(90)] = 4;
        assert_eq!(ret_over(&ring, 100, 10), Some(ret_bps_1e9(4, 5)));
    }

    #[test]
    fn a_steady_uptrend_judges_bull_trend_ext_up_after_confirm() {
        let mut s = state(3, 2);
        // BTC + members all rise 20 bps per minute — ER = 1.0, breadth
        // unanimous, stretch = ret / RV = 10 min × 20 bps / (sqrt(10)·20 bps) ≈ 3.16 > 2.
        let price = |_i: usize, k: i64| 100_000_000 + k * 200_000;
        run_minutes(&mut s, &[BTC, ETH, SOL, XRP], 14, price);
        let w = s.measured(0);
        assert_eq!(w.value_of(DIM_TREND), Some(TREND_BULL));
        assert_eq!(w.value_of(DIM_SHAPE), Some(SHAPE_TREND));
        assert_eq!(w.value_of(DIM_STRETCH), Some(STRETCH_EXT_UP));
        // RV ≈ sqrt(10)·20 bps ≈ 63 bps → between p30 (10) and p70 (100).
        assert_eq!(w.value_of(DIM_VOL), Some(VOL_NORMAL));
        // No funding print yet ⇒ both funding dims unknown-marked.
        assert!(w.dim_unknown(DIM_FUND_SIGN));
        assert!(w.dim_unknown(DIM_FUND_LEVEL));
        assert_eq!(w.source(), 1 << SOURCE_MEASURED);
        assert_eq!(s.effective(0), w);
        let raw = s.raw(0);
        assert_eq!(
            raw.present & (RAW_RET | RAW_ER | RAW_RV | RAW_STRETCH | RAW_BREADTH),
            RAW_RET | RAW_ER | RAW_RV | RAW_STRETCH | RAW_BREADTH
        );
        assert_eq!(raw.er_1e9, 1_000_000_000);
        assert_eq!(raw.breadth_up, 3);
        assert_eq!(raw.breadth_n, 3);
        // Members move exactly with BTC ⇒ INLINE.
        assert_eq!(s.rel_of(0, ETH), REL_INLINE);
        assert_eq!(s.rel_of(0, BTC), REL_UNKNOWN);
        assert_eq!(s.rel_of(0, OTHER), REL_UNKNOWN);
        assert!(s.minutes_judged() >= 14);
    }

    #[test]
    fn breadth_can_veto_the_btc_direction_and_rel_sees_laggards() {
        let mut s = state(3, 1);
        // BTC rises 20 bps/min, members fall 20 bps/min ⇒ NEUTRAL, all LAGGING.
        let price = |i: usize, k: i64| {
            if i == 0 {
                100_000_000 + k * 200_000
            } else {
                100_000_000 - k * 200_000
            }
        };
        run_minutes(&mut s, &[BTC, ETH, SOL, XRP], 12, price);
        let w = s.measured(0);
        assert_eq!(w.value_of(DIM_TREND), Some(TREND_NEUTRAL));
        assert_eq!(s.raw(0).breadth_dn, 3);
        assert_eq!(s.rel_of(0, ETH), REL_LAGGING);
        assert_eq!(s.rel_of(1, SOL), REL_LAGGING);
        // Fewer than half the members present ⇒ TREND unknown-marked.
        let mut s2 = state(3, 1);
        run_minutes(&mut s2, &[BTC, ETH], 12, |_, k| 100_000_000 + k * 200_000);
        assert!(s2.measured(0).dim_unknown(DIM_TREND));
        assert_eq!(s2.raw(0).present & RAW_BREADTH, 0);
        // No members configured ⇒ BTC alone decides.
        let mut s3 = state(0, 1);
        run_minutes(&mut s3, &[BTC], 12, |_, k| 100_000_000 + k * 200_000);
        assert_eq!(s3.measured(0).value_of(DIM_TREND), Some(TREND_BULL));
    }

    #[test]
    fn chop_is_low_efficiency_and_the_bands_hold_state() {
        let mut s = state(0, 1);
        // Saw-tooth: ±50 bps alternating ⇒ net move ~0, path large ⇒ ER ≈ 0.
        let price = |_: usize, k: i64| if k % 2 == 0 { 100_000_000 } else { 100_500_000 };
        run_minutes(&mut s, &[BTC], 14, price);
        assert_eq!(s.measured(0).value_of(DIM_SHAPE), Some(SHAPE_CHOP));
        assert!(s.raw(0).er_1e9 < 300_000_000);
        let pp = short_profile();
        // Band machine: from CHOP, ER 0.32 (≥ lo_enter, < lo_exit) stays CHOP;
        // from MIXED the same ER is MIXED; from TREND ER 0.57 stays TREND.
        assert_eq!(judge_shape(SHAPE_CHOP, 320_000_000, &pp), SHAPE_CHOP);
        assert_eq!(judge_shape(SHAPE_MIXED, 320_000_000, &pp), SHAPE_MIXED);
        assert_eq!(judge_shape(ABSENT, 320_000_000, &pp), SHAPE_MIXED);
        assert_eq!(judge_shape(SHAPE_TREND, 570_000_000, &pp), SHAPE_TREND);
        assert_eq!(judge_shape(SHAPE_MIXED, 570_000_000, &pp), SHAPE_MIXED);
        assert_eq!(judge_shape(SHAPE_CHOP, 700_000_000, &pp), SHAPE_TREND);
        assert_eq!(judge_shape(SHAPE_TREND, 100_000_000, &pp), SHAPE_CHOP);
    }

    #[test]
    fn confirm_law_needs_consecutive_agreeing_minutes() {
        let mut j = Judge::EMPTY;
        assert!(!j.feed(TREND_BULL, 3));
        assert_eq!(j.cur, ABSENT);
        assert!(!j.feed(TREND_BULL, 3));
        assert!(!j.feed(TREND_BEAR, 3)); // interrupted: restart
        assert!(!j.feed(TREND_BULL, 3));
        assert!(!j.feed(TREND_BULL, 3));
        assert!(!j.feed(TREND_BULL, 3)); // ABSENT→BULL is not a flip
        assert_eq!(j.cur, TREND_BULL);
        assert!(!j.feed(TREND_BEAR, 3));
        assert!(!j.feed(TREND_BEAR, 3));
        assert!(j.feed(TREND_BEAR, 3)); // BULL→BEAR is a flip
        assert_eq!(j.cur, TREND_BEAR);
        assert!(!j.feed(ABSENT, 1)); // → unknown, not a flip
        assert_eq!(j.cur, ABSENT);
        // With confirm 1 every change commits at once.
        let mut k = Judge::EMPTY;
        assert!(!k.feed(VOL_LOW, 1));
        assert!(k.feed(VOL_HIGH, 1));
    }

    #[test]
    fn funding_dims_follow_the_latch_and_the_percentiles() {
        let mut s = state(0, 1);
        s.on_funding(-50_000, 1_700_000_000_000);
        run_minutes(&mut s, &[BTC], 3, |_, _| 100_000_000);
        let w = s.measured(1);
        assert_eq!(w.value_of(DIM_FUND_SIGN), Some(FUND_NEG));
        assert_eq!(w.value_of(DIM_FUND_LEVEL), Some(LEVEL_NORMAL)); // within ±100_000
        s.on_funding(250_000, 0); // ms 0 is still a print
        run_minutes(&mut s, &[BTC], 2, |_, _| 100_000_000);
        let w = s.measured(1);
        assert_eq!(w.value_of(DIM_FUND_SIGN), Some(FUND_POS));
        assert_eq!(w.value_of(DIM_FUND_LEVEL), Some(LEVEL_HIGH));
        assert_eq!(s.funding(), (250_000, 1));
        let pp = ProfileParams::FAST_DEFAULT; // percentiles 0 ⇒ ABSENT
        assert_eq!(judge_fund_level(5, &pp), ABSENT);
        assert_eq!(judge_vol(5, &pp), ABSENT);
    }

    #[test]
    fn effective_law_declared_overrides_named_dims_and_expires() {
        let mut s = state(0, 1);
        run_minutes(&mut s, &[BTC], 12, |_, k| 100_000_000 + k * 200_000);
        let measured = s.measured(0);
        assert_eq!(measured.value_of(DIM_TREND), Some(TREND_BULL));
        let now = T0_MONO + 13 * MINUTE_NS;
        // Declare TREND=bear only; other dims stay measured; SOURCE=DECLARED.
        let decl = RegimeWord::EMPTY.with_dim(DIM_TREND, TREND_BEAR);
        s.set_declared(0, decl, now, 5 * MINUTE_NS);
        assert_eq!(s.refresh_effective(now), 0b01);
        let eff = s.effective(0);
        assert_eq!(eff.value_of(DIM_TREND), Some(TREND_BEAR));
        assert_eq!(eff.value_of(DIM_SHAPE), measured.value_of(DIM_SHAPE));
        assert_eq!(eff.source(), 1 << SOURCE_DECLARED);
        assert!(s.declared_fresh(0, now + 4 * MINUTE_NS));
        assert!(!s.declared_fresh(0, now + 5 * MINUTE_NS));
        assert_eq!(s.declared_age_ns(0, now + 7), 7);
        assert_eq!(s.declared_age_ns(1, now), u64::MAX);
        // Disagreement is counted on the next judged minute.
        run_minutes(&mut s, &[BTC], 1, |_, k| 102_400_000 + k * 200_000);
        assert_eq!(s.disagree(0), 1);
        // Expiry: back to measured.
        let later = now + 6 * MINUTE_NS;
        assert_eq!(s.refresh_effective(later), 0b01);
        assert_eq!(s.effective(0).source(), 1 << SOURCE_MEASURED);
        assert_eq!(s.refresh_effective(later), 0);
        // clear_declared drops it immediately.
        s.set_declared(1, decl, later, MINUTE_NS);
        assert!(s.declared_fresh(1, later));
        s.clear_declared(1);
        assert!(!s.declared_fresh(1, later));
        assert_eq!(s.declared(1), RegimeWord::EMPTY);
        // An explicit unknown mark in a declaration forces the dim unknown.
        s.set_declared(
            0,
            RegimeWord::EMPTY.with_dim_unknown(DIM_TREND),
            later,
            MINUTE_NS,
        );
        s.refresh_effective(later);
        assert!(s.effective(0).dim_unknown(DIM_TREND));
        // Out-of-range profile is ignored.
        s.set_declared(7, decl, later, MINUTE_NS);
        assert_eq!(s.declared(7), RegimeWord::EMPTY);
    }

    #[test]
    fn unknown_until_anything_is_known_then_measured() {
        let mut s = state(0, 1);
        assert_eq!(s.effective(0), RegimeWord::UNKNOWN);
        assert_eq!(s.effective(0).source(), 1 << SOURCE_UNKNOWN);
        // Two minutes of data cannot fill a 10-minute window ⇒ still UNKNOWN.
        let changed = run_minutes(&mut s, &[BTC], 2, |_, _| 100_000_000);
        assert_eq!(changed, 0);
        assert_eq!(s.effective(0), RegimeWord::UNKNOWN);
        // A funding print alone makes FUND_SIGN known ⇒ measured, rest unknown-marked.
        s.on_funding(1, 1);
        let changed = run_minutes(&mut s, &[BTC], 1, |_, _| 100_000_000);
        assert_eq!(changed, 0b11);
        let e = s.effective(0);
        assert_eq!(e.source(), 1 << SOURCE_MEASURED);
        assert_eq!(e.value_of(DIM_FUND_SIGN), Some(FUND_POS));
        assert!(e.dim_unknown(DIM_TREND));
        assert!(e.dim_unknown(DIM_VOL));
    }

    #[test]
    fn seed_fills_the_ring_and_warms_the_judges() {
        let mut s = state(3, 3);
        let mut rows = Vec::new();
        let mut k = 0i64;
        while k < 40 {
            let m = M0 - 40 + k;
            for sym in [BTC, ETH, SOL, XRP] {
                rows.push(SeedRow::new(sym, m, 100_000_000 + k * 200_000));
            }
            k += 1;
        }
        // Junk rows: future, non-member, non-positive, too old.
        rows.push(SeedRow::new(BTC, M0, 1));
        rows.push(SeedRow::new(OTHER, M0 - 1, 5));
        rows.push(SeedRow::new(BTC, M0 - 1, 0));
        rows.push(SeedRow::new(BTC, M0 - REGIME_RING_MIN as i64, 5));
        let applied = s.seed(&rows);
        assert_eq!(applied, 160);
        assert_eq!(s.seed_rows(), 160);
        assert_eq!(s.close(SLOT_BTC, M0 - 1), 100_000_000 + 39 * 200_000);
        // Warm at once — the words are valid before the first live minute.
        assert_eq!(s.minutes_judged(), 0);
        assert_eq!(s.measured(0).value_of(DIM_TREND), Some(TREND_BULL));
        assert_eq!(s.measured(1).value_of(DIM_SHAPE), Some(SHAPE_TREND));
        assert_eq!(s.flips(0, DIM_TREND), 0);
        assert_eq!(s.refresh_effective(T0_MONO), 0b11);
        assert_eq!(s.effective(0).source(), 1 << SOURCE_MEASURED);
        // Unconfigured state refuses seeds.
        let mut u = RegimeState::new_boxed();
        assert_eq!(u.seed(&rows), 0);
    }

    #[test]
    fn flips_are_counted_only_between_known_values() {
        let mut s = state(0, 1);
        run_minutes(&mut s, &[BTC], 12, |_, k| 100_000_000 + k * 200_000);
        assert_eq!(s.measured(0).value_of(DIM_TREND), Some(TREND_BULL));
        assert_eq!(s.flips(0, DIM_TREND), 0); // ABSENT→BULL
                                              // Reverse hard: 12 minutes of −20 bps/min.
        run_minutes(&mut s, &[BTC], 12, |_, k| 102_400_000 - k * 200_000);
        assert_eq!(s.measured(0).value_of(DIM_TREND), Some(TREND_BEAR));
        assert!(s.flips(0, DIM_TREND) >= 1);
        assert_eq!(s.flips(9, DIM_TREND), 0);
    }

    #[test]
    fn merge_and_disagree_laws() {
        let m = RegimeWord::from_values(
            TREND_BULL,
            SHAPE_TREND,
            VOL_LOW,
            FUND_POS,
            LEVEL_LOW,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        );
        let d = RegimeWord::EMPTY.with_dim(DIM_VOL, VOL_HIGH);
        let e = merge_declared(d, m);
        assert_eq!(e.value_of(DIM_VOL), Some(VOL_HIGH));
        assert_eq!(e.value_of(DIM_TREND), Some(TREND_BULL));
        assert_eq!(e.source(), 1 << SOURCE_DECLARED);
        assert!(declared_disagrees(d, m));
        assert!(!declared_disagrees(
            RegimeWord::EMPTY.with_dim(DIM_VOL, VOL_LOW),
            m
        ));
        assert!(!declared_disagrees(RegimeWord::EMPTY, m));
        assert!(any_known(m));
        assert!(!any_known(RegimeWord::UNKNOWN));
        assert!(!any_known(RegimeWord::EMPTY));
        // Full declaration replaces everything.
        let full = RegimeWord::from_values(
            TREND_BEAR,
            SHAPE_CHOP,
            VOL_HIGH,
            FUND_NEG,
            LEVEL_HIGH,
            STRETCH_EXT_DOWN,
            SOURCE_MEASURED,
        );
        let full_wire = RegimeWord(full.0 & !(0xFFu64 << (8 * DIM_SOURCE as u32)));
        assert_eq!(
            merge_declared(full_wire, m).0,
            full.with_source(SOURCE_DECLARED).0
        );
    }

    #[test]
    fn candidate_laws() {
        let pp = short_profile();
        assert_eq!(judge_trend(40_000_000_000, 2, 0, 3, 3, &pp), TREND_BULL); // 2/3 ≥ 0.6
        assert_eq!(judge_trend(40_000_000_000, 1, 0, 3, 3, &pp), TREND_NEUTRAL);
        assert_eq!(judge_trend(-40_000_000_000, 0, 3, 3, 3, &pp), TREND_BEAR);
        assert_eq!(judge_trend(-40_000_000_000, 0, 0, 0, 0, &pp), TREND_BEAR); // no members
        assert_eq!(judge_trend(10_000_000_000, 3, 0, 3, 3, &pp), TREND_NEUTRAL);
        assert_eq!(judge_vol(5_000_000_000, &pp), VOL_LOW);
        assert_eq!(judge_vol(50_000_000_000, &pp), VOL_NORMAL);
        assert_eq!(judge_vol(500_000_000_000, &pp), VOL_HIGH);
        assert_eq!(judge_fund_sign(-1), FUND_NEG);
        assert_eq!(judge_fund_sign(0), FUND_POS);
        assert_eq!(judge_fund_level(-200_000, &pp), LEVEL_LOW);
        assert_eq!(judge_stretch(2_000_000_001, &pp), STRETCH_EXT_UP);
        assert_eq!(judge_stretch(-2_000_000_001, &pp), STRETCH_EXT_DOWN);
        assert_eq!(judge_stretch(0, &pp), STRETCH_NEUTRAL);
        assert_eq!(judge_rel(-60_000_000_000, pp.rel_thr_bps_1e9), REL_LAGGING);
        assert_eq!(judge_rel(60_000_000_000, pp.rel_thr_bps_1e9), REL_LEADING);
        assert_eq!(judge_rel(0, pp.rel_thr_bps_1e9), REL_INLINE);
    }

    #[test]
    fn er_and_rv_presence_laws() {
        let mut ring = [0i64; REGIME_RING_MIN];
        // 10-minute window, closes only every 10th minute: the ER
        // steps resolve through the 5-minute walk-back (both present,
        // ER = 1.0), but only 5 of the 10 one-minute returns do ⇒ RV
        // absent (< 80 %).
        ring[ring_idx(80)] = 100;
        ring[ring_idx(90)] = 110;
        ring[ring_idx(100)] = 120;
        assert_eq!(er_over(&ring, 100, 10), Some(1_000_000_000));
        assert_eq!(rv_over(&ring, 100, 10), None);
        // Closes every 5th minute: the walk-back forward-fills every
        // return ⇒ RV present (two non-zero returns).
        ring[ring_idx(95)] = 115;
        ring[ring_idx(85)] = 105;
        let rv = rv_over(&ring, 100, 10).unwrap();
        let r1 = ret_bps_1e9(110, 115) as i128;
        let r2 = ret_bps_1e9(115, 120) as i128;
        assert_eq!(rv, isqrt_i128(r1 * r1 + r2 * r2));
        // Fill every minute: RV present; a flat window is ER 0, RV 0.
        let mut flat = [0i64; REGIME_RING_MIN];
        let mut m = 80;
        while m <= 100 {
            flat[ring_idx(m)] = 100_000_000;
            m += 1;
        }
        assert_eq!(er_over(&flat, 100, 10), Some(0));
        assert_eq!(rv_over(&flat, 100, 10), Some(0));
        assert_eq!(er_over(&flat, 100, 4), None); // < one step
    }

    #[test]
    fn layout_is_boot_boxed_and_bounded() {
        assert!(core::mem::size_of::<RegimeState>() < 450 * 1024);
        assert_eq!(core::mem::align_of::<RegimeState>(), 64);
        assert_eq!(core::mem::size_of::<SeedRow>(), 24);
        assert_eq!(core::mem::size_of::<RegimeRaw>(), 48);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn rv_is_monotone_in_scale_and_er_bounded(scale in 1i64..50, seed in 0u64..1000) {
            let mut ring = [0i64; REGIME_RING_MIN];
            let mut x = seed;
            let mut m = 0i64;
            while m <= 60 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let noise = ((x >> 33) % 2001) as i64 - 1000; // ±1000 ×1e6 = ±10 bps @ 1e8
                ring[ring_idx(m)] = 100_000_000 + noise * scale;
                m += 1;
            }
            let rv = rv_over(&ring, 60, 30).unwrap();
            let er = er_over(&ring, 60, 30).unwrap();
            prop_assert!(rv >= 0);
            prop_assert!((0..=1_000_000_000).contains(&er));
            let mut ring2 = ring;
            let mut m = 0i64;
            while m <= 60 {
                let dev = ring[ring_idx(m)] - 100_000_000;
                ring2[ring_idx(m)] = 100_000_000 + dev * 2;
                m += 1;
            }
            // Doubling deviations at least does not shrink RV.
            prop_assert!(rv_over(&ring2, 60, 30).unwrap() >= rv);
        }

        #[test]
        fn confirm_never_commits_before_confirm_min(confirm in 1u8..6, seq in proptest::collection::vec(0u8..3, 1..40)) {
            let mut j = Judge::EMPTY;
            let mut run = 0u8;
            let mut last = ABSENT;
            for &c in &seq {
                if c == last { run = run.saturating_add(1); } else { run = 1; last = c; }
                let before = j.cur;
                j.feed(c, confirm);
                if j.cur != before {
                    prop_assert!(run >= confirm, "committed after {run} < {confirm}");
                }
            }
        }
    }
}
