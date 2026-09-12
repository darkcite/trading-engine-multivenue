// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-xsd — cross-sectional dislocation member (slot 2)
//!
//! The coded carrier of the statarb lane's surviving finding (doc 07,
//! plan doc 08 §3 — both in the git-excluded research vault): for each
//! TARGET perp, `K` partners with fixed hedge ratios `β_k`; the pairwise
//! spread `s_k = ln P_target − β_k ln P_partner` on HOURLY closes; a
//! causal rolling z over `z_window_h` hours; at every hour boundary a
//! decision per target — ENTER when `#{k : z_k ≥ z_in} ≥ m` (or the
//! mirror), the whole stack EXITs at the first of max-hold / `|z̄| ≥
//! z_stop` / `z̄·d ≤ z_out`, one grid unit ADDed per rung — and one IoC
//! taker order per decision at the first fresh tick after the boundary.
//! Single-leg: only the target is traded; the partners enter through
//! the signal alone.
//!
//! ## Data, not code
//!
//! Like `strategy-vrp`, everything that changes month to month is an
//! artifact the cli resolves at boot: `xsd.toml` (integer parameters,
//! `core-config::xsd`), `xsd-table.tsv` (target / partner / β rows —
//! its hash is the table identity), `xsd-seed.tsv` (hourly closes that
//! warm the rings) and `xsd-state.tsv` (the engine's own positions,
//! restored only under the same table hash). This crate consumes the
//! RESOLVED forms — [`XsdParams`], [`XsdTable`], [`XsdStrategy::seed_close`],
//! [`XsdStrategy::restore_position`] — and never touches a file.
//!
//! ## Clock and bar law
//!
//! Hours are WALL hours through the boot [`WallAnchor`] (the icdp /
//! regime law, replayed identically offline by the harness). An hour's
//! close is the last FRESH mid seen with a stamp inside that hour; an
//! hour with none is an EMPTY bucket for that sym, and every pair whose
//! two legs hold the bucket contributes one spread. Stale ticks
//! (`TICK_FLAG_STALE`, VT3) never enter a bucket and never price an
//! order. The roll happens on the 1 s timer or on the first tick past
//! the boundary, whichever comes first, and evaluates EVERY sym at once
//! — a partner that did not tick leaves an empty bucket rather than a
//! late one (doc 08 C6).
//!
//! ## Decision table (per target, at the roll into hour `H`, on the
//! window ending at `H − 1`)
//!
//! | state | condition | intent |
//! |---|---|---|
//! | flat, `H ≥ last_exit + 1 + cooldown_h` | `#{z_k ≥ z_in} ≥ m` → `d = +1`; else `#{z_k ≤ −z_in} ≥ m` → `d = −1` (ties → `+1`) | ENTER, side = `d · direction` |
//! | pending / entered | `H − entry_hour ≥ max_hold_h` | EXIT max-hold (first, unconditional) |
//! | pending / entered, `n_finite > 0` | `\|z\|̄ ≥ z_stop` | EXIT stop |
//! | pending / entered, `n_finite > 0` | `z̄ · d ≤ z_out` | EXIT revert |
//! | entered, `grid_units < grid_n`, `n_finite > 0` | `\|z\|̄ ≥ z_in + grid_units · Δ` | ADD one unit |
//! | any | no finite `z_k` | HOLD (counted) |
//!
//! A pending intent PERSISTS across rolls until a fresh tick prices it
//! (the research fills at the next executable bar, however far away);
//! a target whose ENTER is still unfilled when an exit fires is
//! CANCELLED (the research books a zero-hold trade there — cost only).
//! Cooldown mirrors the research's `t = x + 1` from the exit FILL: with
//! `cooldown_h = 1` the next entry can be decided two boundaries after
//! the hour the exit was priced in.
//!
//! ## Hot-path rules
//!
//! Zero allocation after [`XsdStrategy::new`] (one boot-time box of
//! ≈ 4.6 MiB); no floats — logs, spreads, z-scores and thresholds are
//! `i64` ×1e9 with `i128` intermediates and floor division
//! (`claude_worker.xsd_ref` runs the same integer law; the parity
//! fixture pins them); `on_tick` is one open-addressed probe, one
//! compare against the precomputed boundary and, for a target with a
//! pending intent, one order; the roll is `O(pairs × window)` integer
//! work off the tick path (384 × 720 ≈ 0.3 M adds per hour).
//! `debug_assert!` on every invariant; release = abort.

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

use core_regime::math::floor_div;
use core_time::{BarClock, NsTs, WallAnchor};
use core_types::{
    symbol_venue_byte, Order, Price, Qty, RegimeLabelSet, Side, SymbolId, Tick, VenueId,
    SYMBOL_ID_NONE,
};
use math::{ln1e9, spread_1e9, z_1e9, WindowStats};
use strategy_core::{Ctx, RegimeGate, Strategy, StrategyCounters, StrategyError, SubmitErr};

/// The counters the engine mirrors and the position view the cli
/// persists (defined in `strategy-core` so the cli never names this
/// crate — the icdp / vrp precedent).
pub use strategy_core::{XsdCounters, XsdPositionView};

// ---------------------------------------------------------------
// Capacities and constants
// ---------------------------------------------------------------

/// Target capacity (the research universe is 110).
pub const XSD_MAX_TARGETS: usize = 128;
/// Partners per target.
pub const XSD_K: usize = 3;
/// Pair capacity.
pub const XSD_MAX_PAIRS: usize = XSD_MAX_TARGETS * XSD_K;
/// Distinct syms (targets ∪ partners).
pub const XSD_MAX_SYMS: usize = 256;
/// Hourly ring depth per sym — the cap on `z_window_h` (90 d).
pub const XSD_RING_H: usize = 2160;
/// Grid depth cap.
pub const XSD_MAX_GRID: u8 = 8;
/// Open-addressed sym map slots (2× the sym capacity).
const MAP_SLOTS: usize = 512;
/// Wall hour in ns.
pub const HOUR_NS: u64 = 3_600_000_000_000;
/// The set's roll poll cadence (the `REGIME_TIMER_NS` precedent).
pub const XSD_TIMER_NS: u64 = 1_000_000_000;
/// `Order.kind` of every order this member emits (the I1 IoC law).
pub use core_fill::ORDER_KIND_IOC;
/// An empty hourly bucket. `ln1e9(1) == 0` is the only log that
/// collides — a price of one micro-dollar, which no perp quotes;
/// [`XsdStrategy::seed_close`] and the roll treat it as absent.
pub const LN_EMPTY: i64 = 0;
/// Risk-policy caps (`docs/risk-policy.md`), from the one shared table.
pub const CAP_LEG_1E6: i64 = strategy_core::CAPS_BASE.leg_usd_1e6;
/// Per-symbol cap USD ×1e6.
pub const CAP_SYM_1E6: i64 = strategy_core::CAPS_BASE.sym_usd_1e6;
/// Book cap USD ×1e6.
pub const CAP_TABLE_1E6: i64 = strategy_core::CAPS_BASE.table_usd_1e6;

/// Target state: flat.
pub const ST_FLAT: u8 = 0;
/// Target state: ENTER decided, not yet priced.
pub const ST_PENDING_ENTER: u8 = 1;
/// Target state: position open.
pub const ST_ENTERED: u8 = 2;

/// No intent pending.
pub const INTENT_NONE: u8 = 0;
/// Pending ENTER.
pub const INTENT_ENTER: u8 = 1;
/// Pending ADD (one grid unit).
pub const INTENT_ADD: u8 = 2;
/// Pending EXIT (the whole stack).
pub const INTENT_EXIT: u8 = 3;

/// Exit reason: `z̄ · d ≤ z_out`.
pub const EXIT_REVERT: u8 = 1;
/// Exit reason: `|z|̄ ≥ z_stop`.
pub const EXIT_STOP: u8 = 2;
/// Exit reason: held `≥ max_hold_h`.
pub const EXIT_MAXHOLD: u8 = 3;
/// Exit reason: restored under a changed table hash (the research's
/// fold end).
pub const EXIT_ROTATION: u8 = 4;
/// Exit reason: the regime gate hard-closed.
pub const EXIT_REGIME: u8 = 5;

const NO_TARGET: u16 = u16::MAX;
const NO_HOUR: i64 = i64::MIN;
const SIDE_LONG: u8 = 1;
const SIDE_SHORT: u8 = 2;

// ---------------------------------------------------------------
// Boot artifacts (resolved forms)
// ---------------------------------------------------------------

/// The member's parameters — `xsd.toml` after `core-config::xsd`,
/// with every duration already in hours / ns. POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct XsdParams {
    /// Rolling z window, hours (`≥ 40`, `≤ XSD_RING_H`).
    pub z_window_h: u32,
    /// Entry threshold ×1e9 (`> 0`).
    pub z_enter_1e9: i64,
    /// Revert exit threshold ×1e9 (`≥ 0`).
    pub z_exit_1e9: i64,
    /// Stop threshold ×1e9 (`> z_enter`).
    pub z_stop_1e9: i64,
    /// Partners that must agree for an entry (`1..=K`).
    pub consensus: u8,
    /// Grid depth (`1..=XSD_MAX_GRID`; 1 = no scale-in).
    pub grid_n: u8,
    /// Grid rung step ×1e9 (`> 0` when `grid_n > 1`).
    pub grid_step_1e9: i64,
    /// Max hold, hours (`> 0`).
    pub max_hold_h: u32,
    /// Cooldown after an exit, hours (`≥ 0`; 1 = the research law).
    pub cooldown_h: u32,
    /// `Order.ttl_ns` of every IoC (`> 0`).
    pub ttl_ns: u64,
    /// Notional per unit, USD ×1e6 (`> 0`, `≤ CAP_LEG_1E6`).
    pub position_usd_1e6: i64,
    /// Max simultaneously entered targets (`> 0`).
    pub max_positions: u16,
    /// Book cap USD ×1e6 (`> 0`, `≤ CAP_TABLE_1E6`).
    pub max_gross_usd_1e6: i64,
    /// `+1` momentum (buy a positive dislocation), `−1` revert.
    pub direction: i8,
    /// Limit offset from the touch, bps ×1e9 (`≥ 0`).
    pub slip_1e9: i64,
    /// SHA-256 of the artifact file (boot log / report header).
    pub hash: [u8; 32],
}

impl XsdParams {
    /// Nothing configured.
    pub const EMPTY: Self = Self {
        z_window_h: 0,
        z_enter_1e9: 0,
        z_exit_1e9: 0,
        z_stop_1e9: 0,
        consensus: 0,
        grid_n: 0,
        grid_step_1e9: 0,
        max_hold_h: 0,
        cooldown_h: 0,
        ttl_ns: 0,
        position_usd_1e6: 0,
        max_positions: 0,
        max_gross_usd_1e6: 0,
        direction: 0,
        slip_1e9: 0,
        hash: [0; 32],
    };
}

/// One resolved table row: target, partner, hedge ratio.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct XsdTableRow {
    /// The traded instrument.
    pub target: SymbolId,
    /// The partner (signal only).
    pub partner: SymbolId,
    /// `β` ×1e9 (`≠ 0`).
    pub beta_1e9: i64,
}

impl XsdTableRow {
    /// An unused row.
    pub const EMPTY: Self = Self {
        target: SYMBOL_ID_NONE,
        partner: SYMBOL_ID_NONE,
        beta_1e9: 0,
    };
}

/// The resolved table: rows in file order (a target's rows contiguous
/// or not — the member groups them), plus the file hash. POD.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct XsdTable {
    /// Rows in use.
    pub n: usize,
    /// Rows (`..n` meaningful).
    pub rows: [XsdTableRow; XSD_MAX_PAIRS],
    /// SHA-256 of `xsd-table.tsv` — the table identity a state file
    /// must match to be restored.
    pub hash: [u8; 32],
}

impl XsdTable {
    /// An empty table.
    pub const EMPTY: Self = Self {
        n: 0,
        rows: [XsdTableRow::EMPTY; XSD_MAX_PAIRS],
        hash: [0; 32],
    };
}

/// One persisted position, as the cli reads it back from
/// `xsd-state.tsv` (descriptors already resolved).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct XsdRestoreRow {
    /// The target.
    pub sym: SymbolId,
    /// `1` long, `2` short.
    pub side: u8,
    /// Entry z-sign (`+1` / `−1`).
    pub d: i8,
    /// Grid units filled (`1..=grid_n`).
    pub grid_units: u8,
    /// Coins ×1e6 held.
    pub qty_1e6: i64,
    /// Notional ×1e6 booked against the caps.
    pub notional_1e6: i64,
    /// The boundary hour of the entry decision.
    pub entry_hour: i64,
    /// The boundary hour of the last add (`entry_hour` when none).
    pub last_add_hour: i64,
}

// ---------------------------------------------------------------
// State
// ---------------------------------------------------------------

/// One sym's hourly log ring and last fresh quote. Deliberately not
/// `Copy`: 17 KiB moves never happen by accident.
#[repr(C, align(64))]
struct SymHours {
    /// `ln1e9(close_1e6)` per wall hour, indexed `hour mod XSD_RING_H`;
    /// [`LN_EMPTY`] = no close.
    ring: [i64; XSD_RING_H],
    /// The newest hour the ring has been advanced to ([`NO_HOUR`] =
    /// never).
    newest_h: i64,
    /// Last fresh mid / touch, ×1e6 (0 = none yet).
    last_mid_1e6: i64,
    last_bid_1e6: i64,
    last_ask_1e6: i64,
    /// Monotonic stamp of that quote.
    last_ts: NsTs,
    sym: SymbolId,
    /// Target index when this sym is traded, else [`NO_TARGET`].
    target: u16,
    venue_byte: u8,
    _pad: [u8; 1],
}

/// One (target, partner, β) pair and its latest z.
#[derive(Copy, Clone)]
#[repr(C)]
struct PairState {
    beta_1e9: i64,
    z_1e9: i64,
    n: u32,
    partner: u16,
    z_valid: u8,
    _pad: [u8; 1],
}

/// One target's machine.
#[derive(Copy, Clone)]
#[repr(C)]
struct TargetState {
    entry_hour: i64,
    last_exit_hour: i64,
    last_add_hour: i64,
    pos_qty_1e6: i64,
    pos_notional_1e6: i64,
    zbar_1e9: i64,
    zmag_1e9: i64,
    sym: u16,
    pairs: [u16; XSD_K],
    k: u8,
    nfin: u8,
    state: u8,
    side: u8,
    d: i8,
    intent: u8,
    exit_reason: u8,
    grid_units: u8,
    _pad: [u8; 2],
}

/// The whole boot-boxed block.
#[repr(C, align(64))]
struct XsdState {
    syms: [SymHours; XSD_MAX_SYMS],
    pairs: [PairState; XSD_MAX_PAIRS],
    targets: [TargetState; XSD_MAX_TARGETS],
    map_sym: [SymbolId; MAP_SLOTS],
    map_idx: [u16; MAP_SLOTS],
    params: XsdParams,
    table_hash: [u8; 32],
    clock: BarClock,
    /// The wall hour currently accumulating ([`NO_HOUR`] until the
    /// first tick or timer).
    cur_hour: i64,
    /// Monotonic end of `cur_hour` (exclusive).
    hour_close_mono: NsTs,
    counters: XsdCounters,
    open_notional_total_1e6: i64,
    n_positions: u32,
    n_syms: u32,
    n_targets: u32,
    n_pairs: u32,
    orders_emitted: u64,
    orders_dropped: u64,
    next_oid: u64,
    state_epoch: u64,
    regime_label: RegimeLabelSet,
    regime_open: bool,
    configured: bool,
}

/// The strategy — a thin owner of one boxed [`XsdState`].
pub struct XsdStrategy {
    st: Box<XsdState>,
}

impl Default for XsdStrategy {
    fn default() -> Self {
        Self::new()
    }
}

/// A test / dashboard view of one target's machine.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct XsdTargetView {
    /// The target sym.
    pub sym: SymbolId,
    /// [`ST_FLAT`] / [`ST_PENDING_ENTER`] / [`ST_ENTERED`].
    pub state: u8,
    /// `1` long, `2` short, `0` none.
    pub side: u8,
    /// Entry z-sign.
    pub d: i8,
    /// Pending intent.
    pub intent: u8,
    /// Pending / last exit reason.
    pub exit_reason: u8,
    /// Grid units (provisional adds included).
    pub grid_units: u8,
    /// Finite partner z-scores at the last roll.
    pub nfin: u8,
    /// `z̄` ×1e9 at the last roll (meaningful when `nfin > 0`).
    pub zbar_1e9: i64,
    /// `|z|̄` ×1e9 at the last roll.
    pub zmag_1e9: i64,
    /// Coins ×1e6 held.
    pub pos_qty_1e6: i64,
    /// Notional ×1e6 booked.
    pub pos_notional_1e6: i64,
    /// Entry decision hour.
    pub entry_hour: i64,
}

/// A test view of one pair's latest z.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct XsdPairView {
    /// Partner sym.
    pub partner: SymbolId,
    /// `β` ×1e9.
    pub beta_1e9: i64,
    /// `z` finite at the last roll.
    pub z_valid: bool,
    /// `z` ×1e9.
    pub z_1e9: i64,
    /// Present samples in the window.
    pub n: u32,
}

/// Sym hash for the open-addressed map (Fibonacci hashing on the
/// namespaced id).
#[inline(always)]
const fn map_home(sym: SymbolId) -> usize {
    ((sym as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 55) as usize & (MAP_SLOTS - 1)
}

/// Price shifted by `bps_1e9` basis points: `px ± px · bps / 1e13`.
#[inline(always)]
fn shift_px_1e6(px_1e6: i64, bps_1e9: i64, up: bool) -> i64 {
    let d = floor_div(px_1e6 as i128 * bps_1e9 as i128, 10_000_000_000_000) as i64;
    if up {
        px_1e6 + d
    } else {
        (px_1e6 - d).max(1)
    }
}

/// Coins ×1e6 for `notional_1e6` USD at `px_1e6` (floor).
#[inline(always)]
fn qty_for_notional_1e6(notional_1e6: i64, px_1e6: i64) -> i64 {
    debug_assert!(px_1e6 > 0);
    floor_div(notional_1e6 as i128 * 1_000_000, px_1e6 as i128) as i64
}

impl XsdStrategy {
    /// Unconfigured strategy (inert until [`Self::configure`]). Boot
    /// path: ONE allocation of the state block, zeroed then given its
    /// sentinels; nothing allocates after this.
    pub fn new() -> Self {
        let layout = core::alloc::Layout::new::<XsdState>();
        // SAFETY: XsdState is a `#[repr(C)]` POD of integers, bools,
        // fixed arrays and the POD `BarClock` / `RegimeLabelSet` /
        // `XsdCounters`; the all-zero pattern is a valid (unconfigured)
        // value, and every non-zero sentinel is written by
        // `reset_sentinels` below before the box is handed out.
        // alloc_zeroed returns memory valid for the layout; Box::from_raw
        // takes sole ownership. A null return aborts at boot.
        let mut st = unsafe {
            let p = std::alloc::alloc_zeroed(layout).cast::<XsdState>();
            assert!(!p.is_null(), "XsdState boot allocation failed");
            Box::from_raw(p)
        };
        st.reset_sentinels();
        Self { st }
    }

    /// Boot-time configuration: the wall anchor, the parameters and
    /// the resolved table. All-or-nothing: a refused artifact leaves
    /// the member unconfigured (`on_start` then errs when the slot is
    /// enabled). Re-configuring resets every ring and position.
    pub fn configure(
        &mut self,
        anchor: WallAnchor,
        params: &XsdParams,
        table: &XsdTable,
    ) -> Result<(), StrategyError> {
        validate_params(params)?;
        if table.n == 0 || table.n > XSD_MAX_PAIRS {
            return Err(StrategyError::Config("xsd: table row count out of range"));
        }
        // Dry run the grouping so a refused table leaves nothing behind.
        let mut probe_syms: [SymbolId; XSD_MAX_SYMS] = [SYMBOL_ID_NONE; XSD_MAX_SYMS];
        let mut probe_targets: [SymbolId; XSD_MAX_TARGETS] = [SYMBOL_ID_NONE; XSD_MAX_TARGETS];
        let mut probe_k: [u8; XSD_MAX_TARGETS] = [0; XSD_MAX_TARGETS];
        let mut n_syms = 0usize;
        let mut n_targets = 0usize;
        let mut r = 0usize;
        while r < table.n {
            let row = table.rows[r];
            if row.target == SYMBOL_ID_NONE || row.partner == SYMBOL_ID_NONE {
                return Err(StrategyError::Config("xsd: unresolved instrument in the table"));
            }
            if row.target == row.partner {
                return Err(StrategyError::Config("xsd: a target cannot partner itself"));
            }
            if row.beta_1e9 == 0 {
                return Err(StrategyError::Config("xsd: zero hedge ratio"));
            }
            let caps = strategy_core::caps_for_sym(row.target);
            if caps.leg_usd_1e6 <= 0 {
                return Err(StrategyError::Config(
                    "xsd: this venue is size-capped, not notional-capped — xsd sizes in USD",
                ));
            }
            if params.position_usd_1e6 > caps.leg_usd_1e6 {
                return Err(StrategyError::Config("xsd: position_usd above the per-order cap"));
            }
            let ti = match find_sym(&probe_targets, n_targets, row.target) {
                Some(i) => i,
                None => {
                    if n_targets == XSD_MAX_TARGETS {
                        return Err(StrategyError::Config("xsd: too many targets"));
                    }
                    probe_targets[n_targets] = row.target;
                    n_targets += 1;
                    n_targets - 1
                }
            };
            if probe_k[ti] as usize == XSD_K {
                return Err(StrategyError::Config("xsd: more than K partners for a target"));
            }
            // Duplicate (target, partner) rows are a malformed table.
            let mut q = 0usize;
            while q < r {
                if table.rows[q].target == row.target && table.rows[q].partner == row.partner {
                    return Err(StrategyError::Config("xsd: duplicate pair row"));
                }
                q += 1;
            }
            probe_k[ti] += 1;
            let mut s = 0usize;
            while s < 2 {
                let sym = if s == 0 { row.target } else { row.partner };
                if find_sym(&probe_syms, n_syms, sym).is_none() {
                    if n_syms == XSD_MAX_SYMS {
                        return Err(StrategyError::Config("xsd: too many distinct syms"));
                    }
                    probe_syms[n_syms] = sym;
                    n_syms += 1;
                }
                s += 1;
            }
            r += 1;
        }
        // ---- commit ----
        let st = &mut *self.st;
        st.reset_sentinels();
        st.params = *params;
        st.table_hash = table.hash;
        st.clock = BarClock::new(anchor, HOUR_NS, 0);
        let mut i = 0usize;
        while i < n_syms {
            let sym = probe_syms[i];
            let s = &mut st.syms[i];
            s.sym = sym;
            s.venue_byte = symbol_venue_byte(sym);
            s.target = NO_TARGET;
            st.map_insert(sym, i as u16);
            i += 1;
        }
        st.n_syms = n_syms as u32;
        let mut t = 0usize;
        while t < n_targets {
            let sym_idx = st.map_lookup(probe_targets[t]).unwrap_or(0);
            st.syms[sym_idx].target = t as u16;
            st.targets[t].sym = sym_idx as u16;
            st.targets[t].k = 0;
            t += 1;
        }
        st.n_targets = n_targets as u32;
        let mut r = 0usize;
        while r < table.n {
            let row = table.rows[r];
            let ti = st.syms[st.map_lookup(row.target).unwrap_or(0)].target as usize;
            let pi = st.map_lookup(row.partner).unwrap_or(0);
            let pair = &mut st.pairs[r];
            pair.beta_1e9 = row.beta_1e9;
            pair.partner = pi as u16;
            pair.z_valid = 0;
            pair.z_1e9 = 0;
            pair.n = 0;
            let tg = &mut st.targets[ti];
            tg.pairs[tg.k as usize] = r as u16;
            tg.k += 1;
            r += 1;
        }
        st.n_pairs = table.n as u32;
        st.configured = true;
        Ok(())
    }

    /// Boot: one hourly close for `sym` at wall hour `hour`
    /// (`close_1e6 > 1`). Fills an empty bucket; never overwrites a
    /// bucket that already holds a close (a tick-fed bucket wins, the
    /// seed-hole law). Returns whether the row landed; a sym outside
    /// the table, an hour older than the ring or a duplicate is counted
    /// in `seed_dropped`.
    pub fn seed_close(&mut self, sym: SymbolId, hour: i64, close_1e6: i64) -> bool {
        let st = &mut *self.st;
        let Some(i) = st.map_lookup(sym) else {
            st.counters.seed_dropped = st.counters.seed_dropped.wrapping_add(1);
            return false;
        };
        if close_1e6 <= 1 {
            st.counters.seed_dropped = st.counters.seed_dropped.wrapping_add(1);
            return false;
        }
        let s = &mut st.syms[i];
        if hour > s.newest_h {
            advance_ring(s, hour);
        } else if s.newest_h - hour >= XSD_RING_H as i64 {
            st.counters.seed_dropped = st.counters.seed_dropped.wrapping_add(1);
            return false;
        }
        let slot = ring_slot(hour);
        if s.ring[slot] != LN_EMPTY {
            st.counters.seed_dropped = st.counters.seed_dropped.wrapping_add(1);
            return false;
        }
        s.ring[slot] = ln1e9(close_1e6 as u64);
        st.counters.seed_rows = st.counters.seed_rows.wrapping_add(1);
        true
    }

    /// Boot: restore one persisted position. `flatten` (the table hash
    /// changed) books it and arms an EXIT `rotation` at the target's
    /// first fresh tick instead of resuming the machine. Fails when the
    /// sym is not a target of the booted table — the cli reports the
    /// orphan; the member cannot flatten what it cannot price.
    pub fn restore_position(&mut self, row: &XsdRestoreRow, flatten: bool) -> Result<(), &'static str> {
        let st = &mut *self.st;
        if !st.configured {
            return Err("xsd: restore before configure");
        }
        let Some(i) = st.map_lookup(row.sym) else {
            return Err("xsd: restored position on a sym outside the table");
        };
        let t = st.syms[i].target;
        if t == NO_TARGET {
            return Err("xsd: restored position on a partner-only sym");
        }
        if row.side != SIDE_LONG && row.side != SIDE_SHORT {
            return Err("xsd: restored side is neither long nor short");
        }
        if row.d != 1 && row.d != -1 {
            return Err("xsd: restored entry sign malformed");
        }
        if row.qty_1e6 <= 0 || row.notional_1e6 <= 0 {
            return Err("xsd: restored size not positive");
        }
        if row.grid_units == 0 || row.grid_units > XSD_MAX_GRID {
            return Err("xsd: restored grid units out of range");
        }
        let tg = &mut st.targets[t as usize];
        if tg.state != ST_FLAT {
            return Err("xsd: duplicate restored position");
        }
        tg.state = ST_ENTERED;
        tg.side = row.side;
        tg.d = row.d;
        tg.grid_units = row.grid_units;
        tg.pos_qty_1e6 = row.qty_1e6;
        tg.pos_notional_1e6 = row.notional_1e6;
        tg.entry_hour = row.entry_hour;
        tg.last_add_hour = row.last_add_hour;
        if flatten {
            tg.intent = INTENT_EXIT;
            tg.exit_reason = EXIT_ROTATION;
        }
        st.open_notional_total_1e6 += row.notional_1e6;
        st.n_positions += 1;
        st.bump_state();
        Ok(())
    }

    // ---- accessors ------------------------------------------------

    /// True after a successful [`Self::configure`].
    #[inline]
    pub fn is_configured(&self) -> bool {
        self.st.configured
    }

    /// Parameter artifact hash.
    #[inline]
    pub fn params_hash(&self) -> &[u8; 32] {
        &self.st.params.hash
    }

    /// Table hash (the state file's identity).
    #[inline]
    pub fn table_hash(&self) -> &[u8; 32] {
        &self.st.table_hash
    }

    /// Targets configured.
    #[inline]
    pub fn targets(&self) -> usize {
        self.st.n_targets as usize
    }

    /// Pairs configured.
    #[inline]
    pub fn pairs(&self) -> usize {
        self.st.n_pairs as usize
    }

    /// Distinct syms (targets ∪ partners).
    #[inline]
    pub fn syms(&self) -> usize {
        self.st.n_syms as usize
    }

    /// Diagnostic counters.
    #[inline]
    pub fn counters(&self) -> &XsdCounters {
        &self.st.counters
    }

    /// Open notional across the book, USD ×1e6.
    #[inline]
    pub fn open_notional_total_1e6(&self) -> i64 {
        self.st.open_notional_total_1e6
    }

    /// Entered targets.
    #[inline]
    pub fn positions(&self) -> u32 {
        self.st.n_positions
    }

    /// Bumped on every persisted-state change (the vrp law: the cli
    /// rewrites `xsd-state.tsv` when it moves).
    #[inline]
    pub fn state_epoch(&self) -> u64 {
        self.st.state_epoch
    }

    /// Copy the entered targets into `out`; returns how many exist
    /// (writes `min(out.len(), that)`). Cold path — the cli's state
    /// writer and `/state`.
    pub fn positions_view(&self, out: &mut [XsdPositionView]) -> u32 {
        let st = &*self.st;
        let mut n = 0u32;
        let mut t = 0usize;
        while t < st.n_targets as usize {
            let tg = &st.targets[t];
            if tg.state == ST_ENTERED {
                if (n as usize) < out.len() {
                    out[n as usize] = XsdPositionView {
                        sym: st.syms[tg.sym as usize].sym,
                        side: tg.side,
                        d: tg.d,
                        grid_units: tg.grid_units,
                        qty_1e6: tg.pos_qty_1e6,
                        notional_1e6: tg.pos_notional_1e6,
                        entry_hour: tg.entry_hour,
                        last_add_hour: tg.last_add_hour,
                    };
                }
                n += 1;
            }
            t += 1;
        }
        n
    }

    /// One target's machine (test / dashboard surface).
    pub fn target_view(&self, t: usize) -> Option<XsdTargetView> {
        let st = &*self.st;
        if t >= st.n_targets as usize {
            return None;
        }
        let tg = &st.targets[t];
        Some(XsdTargetView {
            sym: st.syms[tg.sym as usize].sym,
            state: tg.state,
            side: tg.side,
            d: tg.d,
            intent: tg.intent,
            exit_reason: tg.exit_reason,
            grid_units: tg.grid_units,
            nfin: tg.nfin,
            zbar_1e9: tg.zbar_1e9,
            zmag_1e9: tg.zmag_1e9,
            pos_qty_1e6: tg.pos_qty_1e6,
            pos_notional_1e6: tg.pos_notional_1e6,
            entry_hour: tg.entry_hour,
        })
    }

    /// The `k`-th pair of target `t` (test surface).
    pub fn pair_view(&self, t: usize, k: usize) -> Option<XsdPairView> {
        let st = &*self.st;
        if t >= st.n_targets as usize || k >= st.targets[t].k as usize {
            return None;
        }
        let p = &st.pairs[st.targets[t].pairs[k] as usize];
        Some(XsdPairView {
            partner: st.syms[p.partner as usize].sym,
            beta_1e9: p.beta_1e9,
            z_valid: p.z_valid != 0,
            z_1e9: p.z_1e9,
            n: p.n,
        })
    }

    /// The wall hour the member is accumulating ([`i64::MIN`] before
    /// the first tick / timer).
    #[inline]
    pub fn current_hour(&self) -> i64 {
        self.st.cur_hour
    }
}

/// Linear scan of a boot-time probe array.
fn find_sym(arr: &[SymbolId], n: usize, sym: SymbolId) -> Option<usize> {
    let mut i = 0usize;
    while i < n {
        if arr[i] == sym {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parameter law (doc 08 §3.1 / §3.6).
fn validate_params(p: &XsdParams) -> Result<(), StrategyError> {
    if p.z_window_h < 40 || p.z_window_h as usize > XSD_RING_H {
        return Err(StrategyError::Config("xsd: z_window_h outside [40, ring]"));
    }
    if p.z_enter_1e9 <= 0 || p.z_exit_1e9 < 0 || p.z_stop_1e9 <= p.z_enter_1e9 {
        return Err(StrategyError::Config("xsd: z thresholds malformed (need 0 ≤ exit, 0 < enter < stop)"));
    }
    if p.consensus == 0 || p.consensus as usize > XSD_K {
        return Err(StrategyError::Config("xsd: consensus outside 1..=K"));
    }
    if p.grid_n == 0 || p.grid_n > XSD_MAX_GRID || (p.grid_n > 1 && p.grid_step_1e9 <= 0) {
        return Err(StrategyError::Config("xsd: grid malformed"));
    }
    if p.max_hold_h == 0 || p.ttl_ns == 0 {
        return Err(StrategyError::Config("xsd: max_hold / ttl must be positive"));
    }
    if p.position_usd_1e6 <= 0 || p.position_usd_1e6 > CAP_LEG_1E6 {
        return Err(StrategyError::Config("xsd: position_usd outside (0, per-order cap]"));
    }
    if p.max_positions == 0 || p.max_gross_usd_1e6 <= 0 || p.max_gross_usd_1e6 > CAP_TABLE_1E6 {
        return Err(StrategyError::Config("xsd: max_positions / max_gross malformed"));
    }
    if p.direction != 1 && p.direction != -1 {
        return Err(StrategyError::Config("xsd: direction must be +1 or -1"));
    }
    if p.slip_1e9 < 0 {
        return Err(StrategyError::Config("xsd: slip must be ≥ 0"));
    }
    Ok(())
}

#[inline(always)]
fn ring_slot(hour: i64) -> usize {
    hour.rem_euclid(XSD_RING_H as i64) as usize
}

/// Advance a sym's ring to `hour`, emptying every skipped bucket
/// (bounded by the ring depth) so a slot from an earlier lap can never
/// read as present.
fn advance_ring(s: &mut SymHours, hour: i64) {
    debug_assert!(hour > s.newest_h);
    let gap = if s.newest_h == NO_HOUR {
        XSD_RING_H as i64
    } else {
        (hour - s.newest_h).min(XSD_RING_H as i64)
    };
    let mut j = 0i64;
    while j < gap {
        s.ring[ring_slot(hour - j)] = LN_EMPTY;
        j += 1;
    }
    s.newest_h = hour;
}

impl XsdState {
    fn reset_sentinels(&mut self) {
        let mut i = 0usize;
        while i < MAP_SLOTS {
            self.map_sym[i] = SYMBOL_ID_NONE;
            self.map_idx[i] = 0;
            i += 1;
        }
        let mut s = 0usize;
        while s < XSD_MAX_SYMS {
            let sym = &mut self.syms[s];
            sym.ring = [LN_EMPTY; XSD_RING_H];
            sym.newest_h = NO_HOUR;
            sym.last_mid_1e6 = 0;
            sym.last_bid_1e6 = 0;
            sym.last_ask_1e6 = 0;
            sym.last_ts = 0;
            sym.sym = SYMBOL_ID_NONE;
            sym.target = NO_TARGET;
            sym.venue_byte = 0;
            s += 1;
        }
        let mut t = 0usize;
        while t < XSD_MAX_TARGETS {
            let tg = &mut self.targets[t];
            *tg = TargetState {
                entry_hour: NO_HOUR,
                last_exit_hour: NO_HOUR,
                last_add_hour: NO_HOUR,
                pos_qty_1e6: 0,
                pos_notional_1e6: 0,
                zbar_1e9: 0,
                zmag_1e9: 0,
                sym: 0,
                pairs: [0; XSD_K],
                k: 0,
                nfin: 0,
                state: ST_FLAT,
                side: 0,
                d: 0,
                intent: INTENT_NONE,
                exit_reason: 0,
                grid_units: 0,
                _pad: [0; 2],
            };
            t += 1;
        }
        let mut p = 0usize;
        while p < XSD_MAX_PAIRS {
            self.pairs[p] = PairState {
                beta_1e9: 0,
                z_1e9: 0,
                n: 0,
                partner: 0,
                z_valid: 0,
                _pad: [0; 1],
            };
            p += 1;
        }
        self.params = XsdParams::EMPTY;
        self.table_hash = [0; 32];
        self.clock = BarClock::new(WallAnchor::new(0, 0), HOUR_NS, 0);
        self.cur_hour = NO_HOUR;
        self.hour_close_mono = u64::MAX;
        self.counters = XsdCounters::default();
        self.open_notional_total_1e6 = 0;
        self.n_positions = 0;
        self.n_syms = 0;
        self.n_targets = 0;
        self.n_pairs = 0;
        self.orders_emitted = 0;
        self.orders_dropped = 0;
        self.next_oid = 1;
        self.state_epoch = 0;
        self.regime_label = RegimeLabelSet::ANY;
        self.regime_open = true;
        self.configured = false;
    }

    #[inline(always)]
    fn bump_state(&mut self) {
        self.state_epoch = self.state_epoch.wrapping_add(1);
    }

    fn map_insert(&mut self, sym: SymbolId, idx: u16) {
        let mut h = map_home(sym);
        loop {
            if self.map_sym[h] == SYMBOL_ID_NONE {
                self.map_sym[h] = sym;
                self.map_idx[h] = idx;
                return;
            }
            debug_assert!(self.map_sym[h] != sym, "duplicate sym in the map");
            h = (h + 1) & (MAP_SLOTS - 1);
        }
    }

    /// One probe in the common case; the table is at most half full so
    /// a miss terminates within a short run.
    #[inline(always)]
    fn map_lookup(&self, sym: SymbolId) -> Option<usize> {
        let mut h = map_home(sym);
        loop {
            let v = self.map_sym[h];
            if v == sym {
                return Some(self.map_idx[h] as usize);
            }
            if v == SYMBOL_ID_NONE {
                return None;
            }
            h = (h + 1) & (MAP_SLOTS - 1);
        }
    }

    /// Enter hour `hour` as the accumulating hour (no roll).
    #[inline(always)]
    fn set_hour(&mut self, hour: i64) {
        self.cur_hour = hour;
        self.hour_close_mono = self.clock.close_mono(hour as u64);
    }

    // ---- the roll ------------------------------------------------

    /// Close the hour(s) up to `new_hour − 1`, refresh every pair's z on
    /// the window ending there and run the decision table into
    /// per-target intents. `new_hour > cur_hour`.
    fn roll(&mut self, new_hour: i64) {
        debug_assert!(new_hour > self.cur_hour);
        let closed = new_hour - 1;
        let open_of_closed = self.clock.open_mono(closed as u64);
        let open_of_new = self.clock.open_mono(new_hour as u64);
        // ---- buckets: every sym at once ----
        let mut i = 0usize;
        while i < self.n_syms as usize {
            let s = &mut self.syms[i];
            if closed > s.newest_h {
                advance_ring(s, closed);
            }
            let fresh_in_hour = s.last_mid_1e6 > 1
                && s.last_ts >= open_of_closed
                && s.last_ts < open_of_new;
            if fresh_in_hour {
                s.ring[ring_slot(closed)] = ln1e9(s.last_mid_1e6 as u64);
            }
            i += 1;
        }
        self.set_hour(new_hour);
        self.counters.rolls = self.counters.rolls.wrapping_add(1);
        // ---- pairs: z on the window ending at `closed` ----
        let window = self.params.z_window_h;
        let mut warm = 0u64;
        let mut t = 0usize;
        while t < self.n_targets as usize {
            let tsym = self.targets[t].sym as usize;
            let mut k = 0usize;
            while k < self.targets[t].k as usize {
                let pi = self.targets[t].pairs[k] as usize;
                let partner = self.pairs[pi].partner as usize;
                let beta = self.pairs[pi].beta_1e9;
                let mut w = WindowStats::EMPTY;
                let mut j = 0i64;
                while j < window as i64 {
                    let slot = ring_slot(closed - j);
                    let la = self.syms[tsym].ring[slot];
                    let lb = self.syms[partner].ring[slot];
                    if la != LN_EMPTY && lb != LN_EMPTY {
                        let s = spread_1e9(la, lb, beta);
                        w.push(s);
                        if j == 0 {
                            w.newest_present = true;
                            w.s0 = s;
                        }
                    }
                    j += 1;
                }
                let p = &mut self.pairs[pi];
                p.n = w.n;
                match z_1e9(&w, window) {
                    Some(z) => {
                        p.z_valid = 1;
                        p.z_1e9 = z;
                        warm += 1;
                    }
                    None => {
                        p.z_valid = 0;
                        p.z_1e9 = 0;
                    }
                }
                k += 1;
            }
            t += 1;
        }
        self.counters.pairs_warm = warm;
        // ---- decisions ----
        let mut t = 0usize;
        while t < self.n_targets as usize {
            self.decide(t, new_hour);
            t += 1;
        }
    }

    /// The decision table for target `t` at boundary `hour`.
    fn decide(&mut self, t: usize, hour: i64) {
        let p = self.params;
        // Aggregates over the finite partner z-scores.
        let mut nfin = 0i64;
        let mut sum = 0i128;
        let mut sum_abs = 0i128;
        let mut pos_hit = 0u8;
        let mut neg_hit = 0u8;
        let mut k = 0usize;
        while k < self.targets[t].k as usize {
            let pr = &self.pairs[self.targets[t].pairs[k] as usize];
            if pr.z_valid != 0 {
                nfin += 1;
                sum += pr.z_1e9 as i128;
                sum_abs += pr.z_1e9.unsigned_abs() as i128;
                if pr.z_1e9 >= p.z_enter_1e9 {
                    pos_hit += 1;
                }
                if pr.z_1e9 <= -p.z_enter_1e9 {
                    neg_hit += 1;
                }
            }
            k += 1;
        }
        let tg = &mut self.targets[t];
        tg.nfin = nfin as u8;
        if nfin > 0 {
            tg.zbar_1e9 = floor_div(sum, nfin as i128) as i64;
            tg.zmag_1e9 = floor_div(sum_abs, nfin as i128) as i64;
        } else {
            tg.zbar_1e9 = 0;
            tg.zmag_1e9 = 0;
        }
        self.counters.decisions = self.counters.decisions.wrapping_add(1);
        // ---- an intent nobody priced during the hour is carried, not
        // dropped: the research fills at the next executable bar ----
        if tg.intent != INTENT_NONE {
            self.counters.intents_carried = self.counters.intents_carried.wrapping_add(1);
        }
        match tg.state {
            ST_FLAT => {
                if nfin == 0 {
                    self.counters.holds_absent = self.counters.holds_absent.wrapping_add(1);
                    return;
                }
                let cooled = tg.last_exit_hour == NO_HOUR
                    || hour >= tg.last_exit_hour + 1 + p.cooldown_h as i64;
                if !cooled {
                    return;
                }
                let d: i8 = if pos_hit >= p.consensus {
                    1
                } else if neg_hit >= p.consensus {
                    -1
                } else {
                    return;
                };
                if !self.regime_open {
                    self.counters.regime_blocked = self.counters.regime_blocked.wrapping_add(1);
                    return;
                }
                let long = d * p.direction > 0;
                tg.state = ST_PENDING_ENTER;
                tg.intent = INTENT_ENTER;
                tg.d = d;
                tg.side = if long { SIDE_LONG } else { SIDE_SHORT };
                tg.entry_hour = hour;
                tg.last_add_hour = hour;
                tg.grid_units = 1;
                self.counters.entries_decided = self.counters.entries_decided.wrapping_add(1);
            }
            _ => {
                // ---- exits, in the research's precedence ----
                let held = hour - tg.entry_hour;
                let reason = if held >= p.max_hold_h as i64 {
                    EXIT_MAXHOLD
                } else if nfin > 0 && tg.zmag_1e9 >= p.z_stop_1e9 {
                    EXIT_STOP
                } else if nfin > 0 && tg.zbar_1e9 as i128 * tg.d as i128 <= p.z_exit_1e9 as i128 {
                    EXIT_REVERT
                } else {
                    0
                };
                if reason != 0 {
                    if tg.intent == INTENT_EXIT {
                        return; // an earlier exit is still waiting for a fresh tick
                    }
                    if tg.state == ST_PENDING_ENTER {
                        // Unfilled entry superseded by its own exit signal.
                        tg.state = ST_FLAT;
                        tg.intent = INTENT_NONE;
                        tg.entry_hour = NO_HOUR;
                        tg.grid_units = 0;
                        tg.last_exit_hour = hour;
                        self.counters.entries_cancelled =
                            self.counters.entries_cancelled.wrapping_add(1);
                        return;
                    }
                    tg.intent = INTENT_EXIT;
                    tg.exit_reason = reason;
                    return;
                }
                if nfin == 0 {
                    self.counters.holds_absent = self.counters.holds_absent.wrapping_add(1);
                    return;
                }
                if tg.state == ST_ENTERED
                    && tg.intent == INTENT_NONE
                    && tg.grid_units < p.grid_n
                    && hour > tg.last_add_hour
                {
                    let rung = p.z_enter_1e9 as i128 + tg.grid_units as i128 * p.grid_step_1e9 as i128;
                    if tg.zmag_1e9 as i128 >= rung {
                        tg.intent = INTENT_ADD;
                        tg.grid_units += 1;
                        tg.last_add_hour = hour;
                        self.counters.adds_decided = self.counters.adds_decided.wrapping_add(1);
                    }
                }
            }
        }
    }

    // ---- emission ------------------------------------------------

    #[inline(always)]
    fn emit<C: Ctx>(
        &mut self,
        ctx: &mut C,
        sym_idx: usize,
        side: Side,
        px: i64,
        qty: i64,
        now: NsTs,
    ) -> bool {
        debug_assert!(px > 0 && qty > 0);
        let s = &self.syms[sym_idx];
        let venue = match VenueId::from_u8(s.venue_byte) {
            Some(v) => v,
            None => {
                debug_assert!(false, "configured sym with an undecodable venue byte");
                return false;
            }
        };
        let order = Order::new(
            now,
            venue,
            s.sym,
            side,
            ORDER_KIND_IOC,
            Price::from_raw(px),
            Qty::from_raw(qty),
            self.next_oid,
        )
        .with_ttl_ns(self.params.ttl_ns);
        self.next_oid = self.next_oid.wrapping_add(1);
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

    /// Price the pending intent of the target behind `sym_idx` at its
    /// fresh touch. The paper law: the logical position advances on the
    /// accepted submit (the VM's law; the fill law is the harness's
    /// source of truth).
    fn act<C: Ctx>(&mut self, ctx: &mut C, sym_idx: usize, now: NsTs) {
        let t = self.syms[sym_idx].target as usize;
        let intent = self.targets[t].intent;
        let bid = self.syms[sym_idx].last_bid_1e6;
        let ask = self.syms[sym_idx].last_ask_1e6;
        let p = self.params;
        match intent {
            INTENT_ENTER | INTENT_ADD => {
                let long = self.targets[t].side == SIDE_LONG;
                let (side, px) = if long {
                    (Side::Bid, shift_px_1e6(ask, p.slip_1e9, true))
                } else {
                    (Side::Ask, shift_px_1e6(bid, p.slip_1e9, false))
                };
                let qty = qty_for_notional_1e6(p.position_usd_1e6, px);
                let book_cap = if p.max_gross_usd_1e6 < CAP_TABLE_1E6 {
                    p.max_gross_usd_1e6
                } else {
                    CAP_TABLE_1E6
                };
                let tg = &self.targets[t];
                let cap_ok = qty > 0
                    && tg.pos_notional_1e6 + p.position_usd_1e6 <= CAP_SYM_1E6
                    && self.open_notional_total_1e6 + p.position_usd_1e6 <= book_cap
                    && (intent == INTENT_ADD || self.n_positions < p.max_positions as u32);
                if !cap_ok {
                    self.counters.caps_rejected = self.counters.caps_rejected.wrapping_add(1);
                    let tg = &mut self.targets[t];
                    if intent == INTENT_ENTER {
                        tg.state = ST_FLAT;
                        tg.entry_hour = NO_HOUR;
                        tg.grid_units = 0;
                    } else {
                        tg.grid_units -= 1;
                    }
                    tg.intent = INTENT_NONE;
                    return;
                }
                if !self.emit(ctx, sym_idx, side, px, qty, now) {
                    // Ring full: the intent stays pending for the next tick.
                    return;
                }
                let tg = &mut self.targets[t];
                tg.intent = INTENT_NONE;
                tg.pos_qty_1e6 += qty;
                tg.pos_notional_1e6 += p.position_usd_1e6;
                self.open_notional_total_1e6 += p.position_usd_1e6;
                if intent == INTENT_ENTER {
                    tg.state = ST_ENTERED;
                    self.n_positions += 1;
                    self.counters.entries = self.counters.entries.wrapping_add(1);
                } else {
                    self.counters.adds = self.counters.adds.wrapping_add(1);
                }
                self.bump_state();
            }
            INTENT_EXIT => {
                let tg = self.targets[t];
                let (side, px) = if tg.side == SIDE_LONG {
                    (Side::Ask, shift_px_1e6(bid, p.slip_1e9, false))
                } else {
                    (Side::Bid, shift_px_1e6(ask, p.slip_1e9, true))
                };
                if tg.pos_qty_1e6 > 0 && !self.emit(ctx, sym_idx, side, px, tg.pos_qty_1e6, now) {
                    return;
                }
                self.open_notional_total_1e6 -= tg.pos_notional_1e6;
                debug_assert!(self.open_notional_total_1e6 >= 0);
                self.n_positions -= 1;
                match tg.exit_reason {
                    EXIT_REVERT => self.counters.exits_revert = self.counters.exits_revert.wrapping_add(1),
                    EXIT_STOP => self.counters.exits_stop = self.counters.exits_stop.wrapping_add(1),
                    EXIT_MAXHOLD => {
                        self.counters.exits_maxhold = self.counters.exits_maxhold.wrapping_add(1)
                    }
                    EXIT_ROTATION => {
                        self.counters.exits_rotation = self.counters.exits_rotation.wrapping_add(1)
                    }
                    _ => self.counters.exits_regime = self.counters.exits_regime.wrapping_add(1),
                }
                let exit_hour = self.cur_hour;
                let tg = &mut self.targets[t];
                tg.state = ST_FLAT;
                tg.intent = INTENT_NONE;
                tg.side = 0;
                tg.grid_units = 0;
                tg.pos_qty_1e6 = 0;
                tg.pos_notional_1e6 = 0;
                tg.entry_hour = NO_HOUR;
                // The research's `t = x + 1` counts from the exit FILL bar.
                tg.last_exit_hour = exit_hour;
                self.bump_state();
            }
            _ => {}
        }
    }
}

impl StrategyCounters for XsdStrategy {
    #[inline]
    fn orders_emitted(&self) -> u64 {
        self.st.orders_emitted
    }
    #[inline]
    fn orders_dropped(&self) -> u64 {
        self.st.orders_dropped
    }
    #[inline]
    fn strategy_kind(&self) -> &'static str {
        "xsd"
    }
    #[inline]
    fn xsd_counters(&self) -> XsdCounters {
        self.st.counters
    }
    #[inline]
    fn xsd_state_epoch(&self) -> u64 {
        self.st.state_epoch
    }
    fn xsd_positions_view(&self, out: &mut [XsdPositionView]) -> u32 {
        self.positions_view(out)
    }
}

impl Strategy for XsdStrategy {
    /// Enabled without an artifact ⇒ refuse the boot.
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if !self.st.configured {
            return Err(StrategyError::Config("xsd: enabled without a resolved xsd.toml + table"));
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        let st = &mut *self.st;
        let Some(i) = st.map_lookup(tick.sym) else {
            return;
        };
        let now = tick.ts_ns;
        if st.cur_hour == NO_HOUR {
            let h = st.clock.bar_id(now) as i64;
            st.set_hour(h);
        } else if now >= st.hour_close_mono {
            let h = st.clock.bar_id(now) as i64;
            st.roll(h);
        }
        let bid = tick.bid_px.raw();
        let ask = tick.ask_px.raw();
        if tick.is_stale() || bid <= 0 || ask <= 0 {
            return;
        }
        let s = &mut st.syms[i];
        s.last_bid_1e6 = bid;
        s.last_ask_1e6 = ask;
        s.last_mid_1e6 = (bid + ask) >> 1;
        s.last_ts = now;
        let t = s.target;
        if t != NO_TARGET && st.targets[t as usize].intent != INTENT_NONE {
            st.act(ctx, i, now);
        }
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, _signal: &core_types::Signal, _ctx: &mut C) {}

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, _fill: &core_types::Fill, _ctx: &mut C) {}

    /// The roll poll: a boundary crossed with no tick of any table sym
    /// since still closes the hour on time.
    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, _ctx: &mut C) {
        let st = &mut *self.st;
        if !st.configured {
            return;
        }
        if st.cur_hour == NO_HOUR {
            let h = st.clock.bar_id(now_ns) as i64;
            st.set_hour(h);
        } else if now_ns >= st.hour_close_mono {
            let h = st.clock.bar_id(now_ns) as i64;
            st.roll(h);
        }
    }

    fn timer_period_ns(&self) -> u64 {
        if self.st.configured {
            XSD_TIMER_NS
        } else {
            u64::MAX
        }
    }

    #[inline]
    fn regime_label(&self) -> RegimeLabelSet {
        self.st.regime_label
    }

    #[inline]
    fn set_regime_label(&mut self, set: RegimeLabelSet) -> bool {
        self.st.regime_label = set;
        true
    }

    /// RG2 gate: closed ⇒ no entries (exits and adds drain by the
    /// table); hard-closed ⇒ every entered target is armed to EXIT at
    /// its next fresh tick and every unfilled entry is cancelled.
    fn on_regime<C: Ctx>(&mut self, gate: RegimeGate, _ctx: &mut C) {
        let st = &mut *self.st;
        st.regime_open = gate.open;
        if !gate.hard_closed() || !st.configured {
            return;
        }
        let hour = st.cur_hour;
        let mut t = 0usize;
        while t < st.n_targets as usize {
            let tg = &mut st.targets[t];
            match tg.state {
                ST_ENTERED => {
                    if tg.intent != INTENT_EXIT {
                        tg.intent = INTENT_EXIT;
                        tg.exit_reason = EXIT_REGIME;
                    }
                }
                ST_PENDING_ENTER => {
                    tg.state = ST_FLAT;
                    tg.intent = INTENT_NONE;
                    tg.entry_hour = NO_HOUR;
                    tg.grid_units = 0;
                    tg.last_exit_hour = hour;
                    st.counters.entries_cancelled = st.counters.entries_cancelled.wrapping_add(1);
                }
                _ => {}
            }
            t += 1;
        }
    }

    /// Positions persist across a restart by design (the state file);
    /// nothing is flattened here.
    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::SCALE_1E9;
    use core_types::{make_symbol_id, TICK_FLAG_STALE};

    const MONO0: u64 = 5_000_000_000_000_000;
    /// A wall instant on an exact hour boundary: 2026-09-12 00:00:00Z.
    const WALL0: u64 = 1_789_171_200 * 1_000_000_000;
    const HOUR0: i64 = (WALL0 / HOUR_NS) as i64;

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

    // Binance USDⓈ-M perps live in the Binance namespace from ordinal 512.
    const A: SymbolId = make_symbol_id(VenueId::Binance, 512 + 3);
    const B: SymbolId = make_symbol_id(VenueId::Binance, 512 + 4);
    const C: SymbolId = make_symbol_id(VenueId::Binance, 512 + 5);
    const D: SymbolId = make_symbol_id(VenueId::Binance, 512 + 6);
    /// Seeded history per test: `SEED_H` hours ending at `HOUR0 − 1`, so
    /// the first live close makes the window exactly `z_window_h` deep.
    const SEED_H: i64 = 95;
    const WINDOW_H: u32 = 96;

    fn params() -> XsdParams {
        XsdParams {
            z_window_h: WINDOW_H,
            z_enter_1e9: 3 * SCALE_1E9,
            z_exit_1e9: 0,
            z_stop_1e9: 5 * SCALE_1E9,
            consensus: 1,
            grid_n: 1,
            grid_step_1e9: SCALE_1E9 / 2,
            max_hold_h: 240,
            cooldown_h: 1,
            ttl_ns: 300_000_000_000,
            position_usd_1e6: 1_000_000_000, // $1,000
            max_positions: 82,
            max_gross_usd_1e6: CAP_TABLE_1E6,
            direction: 1,
            slip_1e9: 0,
            hash: [7; 32],
        }
    }

    fn table(rows: &[(SymbolId, SymbolId, i64)]) -> XsdTable {
        let mut t = XsdTable::EMPTY;
        for (i, r) in rows.iter().enumerate() {
            t.rows[i] = XsdTableRow {
                target: r.0,
                partner: r.1,
                beta_1e9: r.2,
            };
        }
        t.n = rows.len();
        t.hash = [9; 32];
        t
    }

    fn anchor() -> WallAnchor {
        WallAnchor::new(MONO0, WALL0)
    }

    fn configured(rows: &[(SymbolId, SymbolId, i64)]) -> XsdStrategy {
        let mut s = XsdStrategy::new();
        s.configure(anchor(), &params(), &table(rows)).expect("configure");
        s
    }

    fn mono_at(hour: i64, offset_ns: u64) -> NsTs {
        MONO0 + (hour - HOUR0) as u64 * HOUR_NS + offset_ns
    }

    fn tick(sym: SymbolId, ts: NsTs, px_1e6: i64, stale: bool) -> Tick {
        let mut t = Tick::new(
            ts,
            VenueId::Binance,
            sym,
            0,
            Price::from_raw(px_1e6),
            Qty::from_raw(1_000_000),
            Price::from_raw(px_1e6),
            Qty::from_raw(1_000_000),
        );
        if stale {
            t.flags |= TICK_FLAG_STALE;
        }
        t
    }

    /// Seed A and B flat (both 100.0) for `n` hours ending at `last`.
    fn seed_flat(s: &mut XsdStrategy, last: i64, n: i64) {
        let mut h = last - n + 1;
        while h <= last {
            assert!(s.seed_close(A, h, 100_000_000));
            assert!(s.seed_close(B, h, 100_000_000));
            h += 1;
        }
    }

    // ---- configure ----

    #[test]
    fn configure_groups_rows_and_refuses_malformations() {
        let s = configured(&[(A, B, SCALE_1E9), (A, C, SCALE_1E9), (D, B, SCALE_1E9)]);
        assert!(s.is_configured());
        assert_eq!(s.targets(), 2);
        assert_eq!(s.pairs(), 3);
        assert_eq!(s.syms(), 4);
        assert_eq!(s.pair_view(0, 1).unwrap().partner, C);
        assert_eq!(s.pair_view(1, 0).unwrap().partner, B);
        assert!(s.pair_view(1, 1).is_none());
        assert_eq!(*s.table_hash(), [9; 32]);
        assert_eq!(*s.params_hash(), [7; 32]);

        let mut bad = XsdStrategy::new();
        // Self-pair.
        assert!(bad.configure(anchor(), &params(), &table(&[(A, A, 1)])).is_err());
        // Zero β.
        assert!(bad.configure(anchor(), &params(), &table(&[(A, B, 0)])).is_err());
        // Duplicate row.
        assert!(bad
            .configure(anchor(), &params(), &table(&[(A, B, 1), (A, B, 2)]))
            .is_err());
        // Four partners.
        assert!(bad
            .configure(
                anchor(),
                &params(),
                &table(&[(A, B, 1), (A, C, 1), (A, D, 1), (A, make_symbol_id(VenueId::Okx, 1), 1)])
            )
            .is_err());
        // Empty table.
        assert!(bad.configure(anchor(), &params(), &XsdTable::EMPTY).is_err());
        // Deribit target: size-capped venue refused.
        let der = make_symbol_id(VenueId::Deribit, 1);
        assert!(bad.configure(anchor(), &params(), &table(&[(der, B, 1)])).is_err());
        assert!(!bad.is_configured());
        // Parameter law.
        let mut p = params();
        p.z_stop_1e9 = p.z_enter_1e9;
        assert!(bad.configure(anchor(), &p, &table(&[(A, B, 1)])).is_err());
        let mut p = params();
        p.position_usd_1e6 = CAP_LEG_1E6 + 1;
        assert!(bad.configure(anchor(), &p, &table(&[(A, B, 1)])).is_err());
        let mut p = params();
        p.direction = 0;
        assert!(bad.configure(anchor(), &p, &table(&[(A, B, 1)])).is_err());
        let mut p = params();
        p.grid_n = 3;
        p.grid_step_1e9 = 0;
        assert!(bad.configure(anchor(), &p, &table(&[(A, B, 1)])).is_err());
        let mut p = params();
        p.z_window_h = XSD_RING_H as u32 + 1;
        assert!(bad.configure(anchor(), &p, &table(&[(A, B, 1)])).is_err());
    }

    #[test]
    fn on_start_refuses_unconfigured() {
        let mut s = XsdStrategy::new();
        assert!(matches!(s.on_start(&mut ctx()), Err(StrategyError::Config(_))));
        assert_eq!(s.timer_period_ns(), u64::MAX);
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        assert!(s.on_start(&mut ctx()).is_ok());
        assert_eq!(s.timer_period_ns(), XSD_TIMER_NS);
    }

    // ---- seeds and the ring ----

    #[test]
    fn seed_fills_empty_buckets_only_and_drops_the_rest() {
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        assert!(s.seed_close(A, HOUR0 - 5, 100_000_000));
        // Duplicate hour: dropped.
        assert!(!s.seed_close(A, HOUR0 - 5, 101_000_000));
        // Older than the ring: dropped.
        assert!(!s.seed_close(A, HOUR0 - 5 - XSD_RING_H as i64, 100_000_000));
        // Backfill inside the ring: lands.
        assert!(s.seed_close(A, HOUR0 - 6, 100_000_000));
        // Unknown sym / micro-dollar close: dropped.
        assert!(!s.seed_close(make_symbol_id(VenueId::Okx, 9), HOUR0, 100_000_000));
        assert!(!s.seed_close(A, HOUR0 - 4, 1));
        assert_eq!(s.counters().seed_rows, 2);
        assert_eq!(s.counters().seed_dropped, 4);
    }

    #[test]
    fn advance_clears_skipped_buckets_across_a_lap() {
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        let mut h = HOUR0 - 60;
        while h < HOUR0 {
            assert!(s.seed_close(A, h, 100_000_000));
            h += 1;
        }
        // (`ring` is private to the crate — the test reads it directly.)
        // Jump more than a lap ahead: every earlier bucket must read empty.
        let far = HOUR0 + XSD_RING_H as i64 + 3;
        assert!(s.seed_close(A, far, 100_000_000));
        let ring = &s.st.syms[0].ring;
        let mut present = 0usize;
        let mut i = 0usize;
        while i < XSD_RING_H {
            if ring[i] != LN_EMPTY {
                present += 1;
            }
            i += 1;
        }
        assert_eq!(present, 1, "only the far bucket survives the lap");
    }

    // ---- the roll and the decision table ----

    /// Drive: seed `SEED_H` flat hours ending at HOUR0−1, then live
    /// ticks in hour HOUR0 with A at `a_px` while B stays 100, and roll
    /// into HOUR0+1 through the timer.
    fn dislocate(s: &mut XsdStrategy, c: &mut RecCtx, a_px: i64) {
        seed_flat(s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), c);
        assert_eq!(s.current_hour(), HOUR0);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), a_px, false), c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), c);
        s.on_timer(mono_at(HOUR0 + 1, 1), c);
        assert_eq!(s.current_hour(), HOUR0 + 1);
    }

    /// Closes alternate 100.0 / 100.2 on A so the spread has variance; B
    /// is flat at 100. A later jump `J` in the log then scores
    /// `z ≈ √(n − 1)` on its first hour and `≈ √(n/2 − 1)` on its second
    /// (the outlier is inside its own window) — 9.7 and 6.9 at n = 96.
    fn seed_wobble(s: &mut XsdStrategy, last: i64, n: i64) {
        let mut h = last - n + 1;
        let mut k = 0i64;
        while h <= last {
            let a = if k % 2 == 0 { 100_000_000 } else { 100_200_000 };
            assert!(s.seed_close(A, h, a));
            assert!(s.seed_close(B, h, 100_000_000));
            h += 1;
            k += 1;
        }
    }

    #[test]
    fn flat_window_has_no_finite_z_and_holds() {
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        let mut c = ctx();
        // The live hour closes flat too: 96 identical spreads ⇒ std 0.
        dislocate(&mut s, &mut c, 100_000_000);
        let pv = s.pair_view(0, 0).unwrap();
        assert!(!pv.z_valid, "zero std ⇒ z absent");
        assert_eq!(pv.n, WINDOW_H);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.nfin, 0);
        assert_eq!(tv.state, ST_FLAT);
        assert_eq!(s.counters().holds_absent, 1);
        assert!(c.orders.is_empty());
    }

    #[test]
    fn entry_add_and_revert_exit_follow_the_table() {
        let mut p = params();
        p.grid_n = 3;
        p.z_stop_1e9 = 1_000 * SCALE_1E9; // the stop has its own test
        let mut s = XsdStrategy::new();
        s.configure(anchor(), &p, &table(&[(A, B, SCALE_1E9)])).unwrap();
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        // Hour HOUR0: A jumps 5 % — a huge positive z on the ±0.1 % history.
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 1, 1), &mut c);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.nfin, 1);
        assert!(tv.zbar_1e9 > 3 * SCALE_1E9, "z̄ = {}", tv.zbar_1e9);
        assert_eq!(tv.state, ST_PENDING_ENTER);
        assert_eq!(tv.intent, INTENT_ENTER);
        assert_eq!(tv.d, 1);
        assert_eq!(tv.side, SIDE_LONG, "momentum buys a positive dislocation");
        assert_eq!(tv.entry_hour, HOUR0 + 1);
        assert!(c.orders.is_empty(), "nothing is priced at the roll");
        // A stale tick does not price it; the first fresh one does, at the ask.
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 5), 105_100_000, true), &mut c);
        assert!(c.orders.is_empty());
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 6), 105_100_000, false), &mut c);
        assert_eq!(c.orders.len(), 1);
        let o = c.orders[0];
        assert_eq!(o.sym, A);
        assert_eq!(o.side, Side::Bid);
        assert_eq!(o.kind, ORDER_KIND_IOC);
        assert_eq!(o.ttl_ns, p.ttl_ns);
        assert_eq!(o.px.raw(), 105_100_000);
        assert_eq!(o.qty.raw(), qty_for_notional_1e6(p.position_usd_1e6, 105_100_000));
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.state, ST_ENTERED);
        assert_eq!(tv.grid_units, 1);
        assert_eq!(s.positions(), 1);
        assert_eq!(s.open_notional_total_1e6(), p.position_usd_1e6);
        assert_eq!(s.counters().entries, 1);
        let epoch = s.state_epoch();
        assert!(epoch > 0);
        // Hour HOUR0+1 closes still dislocated ⇒ |z̄| ≥ z_in + 1·Δ ⇒ ADD.
        s.on_tick(&tick(B, mono_at(HOUR0 + 1, 7), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 2, 1), &mut c);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.intent, INTENT_ADD);
        assert_eq!(tv.grid_units, 2);
        s.on_tick(&tick(A, mono_at(HOUR0 + 2, 5), 105_000_000, false), &mut c);
        assert_eq!(c.orders.len(), 2);
        assert_eq!(c.orders[1].side, Side::Bid);
        assert_eq!(s.counters().adds, 1);
        assert_eq!(s.open_notional_total_1e6(), 2 * p.position_usd_1e6);
        // The dislocation collapses: A back to 100 ⇒ z̄ ≤ 0 ⇒ EXIT revert.
        s.on_tick(&tick(A, mono_at(HOUR0 + 2, 6), 100_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0 + 2, 7), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 3, 1), &mut c);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.intent, INTENT_EXIT);
        assert_eq!(tv.exit_reason, EXIT_REVERT);
        // Priced at the bid, the whole stack.
        s.on_tick(&tick(A, mono_at(HOUR0 + 3, 5), 100_000_000, false), &mut c);
        assert_eq!(c.orders.len(), 3);
        let x = c.orders[2];
        assert_eq!(x.side, Side::Ask);
        assert_eq!(x.qty.raw(), c.orders[0].qty.raw() + c.orders[1].qty.raw());
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.state, ST_FLAT);
        assert_eq!(s.positions(), 0);
        assert_eq!(s.open_notional_total_1e6(), 0);
        assert_eq!(s.counters().exits_revert, 1);
        assert!(s.state_epoch() > epoch);
        // Cooldown: the next boundary (HOUR0+4) may not enter even on a
        // fresh, larger dislocation; HOUR0+5 may.
        s.on_tick(&tick(A, mono_at(HOUR0 + 3, 6), 110_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0 + 3, 7), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 4, 1), &mut c);
        let tv = s.target_view(0).unwrap();
        assert!(tv.zbar_1e9 >= 3 * SCALE_1E9, "the signal is there: z̄ = {}", tv.zbar_1e9);
        assert_eq!(tv.state, ST_FLAT, "cooldown holds");
        s.on_tick(&tick(A, mono_at(HOUR0 + 4, 6), 110_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0 + 4, 7), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 5, 1), &mut c);
        assert_eq!(s.target_view(0).unwrap().state, ST_PENDING_ENTER);
    }

    #[test]
    fn revert_direction_sells_a_positive_dislocation() {
        let mut p = params();
        p.direction = -1;
        let mut s = XsdStrategy::new();
        s.configure(anchor(), &p, &table(&[(A, B, SCALE_1E9)])).unwrap();
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 1, 1), &mut c);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.d, 1);
        assert_eq!(tv.side, SIDE_SHORT);
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 6), 105_000_000, false), &mut c);
        assert_eq!(c.orders[0].side, Side::Ask);
    }

    #[test]
    fn stop_beats_revert_and_maxhold_beats_both() {
        let mut p = params();
        p.max_hold_h = 2;
        let mut s = XsdStrategy::new();
        s.configure(anchor(), &p, &table(&[(A, B, SCALE_1E9)])).unwrap();
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 1, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 6), 105_000_000, false), &mut c);
        assert_eq!(s.target_view(0).unwrap().state, ST_ENTERED);
        // Still far out at HOUR0+2 ⇒ |z̄| ≥ 5 ⇒ stop (held 1 < 2).
        s.on_tick(&tick(B, mono_at(HOUR0 + 1, 7), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 2, 1), &mut c);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.intent, INTENT_EXIT);
        assert_eq!(tv.exit_reason, EXIT_STOP);
        // No fresh tick for A during HOUR0+2: the exit persists into the
        // next roll and max-hold does not overwrite the earlier reason.
        s.on_tick(&tick(B, mono_at(HOUR0 + 2, 7), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 3, 1), &mut c);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.intent, INTENT_EXIT);
        assert_eq!(tv.exit_reason, EXIT_STOP);
        s.on_tick(&tick(A, mono_at(HOUR0 + 3, 6), 105_000_000, false), &mut c);
        assert_eq!(s.counters().exits_stop, 1);
        assert_eq!(s.target_view(0).unwrap().state, ST_FLAT);
    }

    #[test]
    fn maxhold_fires_from_the_entry_decision_hour() {
        let mut p = params();
        p.max_hold_h = 3;
        p.z_stop_1e9 = 1_000 * SCALE_1E9;
        let mut s = XsdStrategy::new();
        s.configure(anchor(), &p, &table(&[(A, B, SCALE_1E9)])).unwrap();
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 1, 1), &mut c); // entry decided at HOUR0+1
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 6), 105_000_000, false), &mut c);
        let mut h = HOUR0 + 1;
        while h < HOUR0 + 4 {
            s.on_tick(&tick(A, mono_at(h, 100), 105_000_000, false), &mut c);
            s.on_tick(&tick(B, mono_at(h, 101), 100_000_000, false), &mut c);
            s.on_timer(mono_at(h + 1, 1), &mut c);
            h += 1;
        }
        // Rolls at HOUR0+2 (held 1), +3 (held 2), +4 (held 3 ⇒ max-hold).
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.intent, INTENT_EXIT);
        assert_eq!(tv.exit_reason, EXIT_MAXHOLD);
    }

    #[test]
    fn unfilled_entry_is_carried_across_rolls_and_cancelled_by_an_exit() {
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 1, 1), &mut c);
        assert_eq!(s.target_view(0).unwrap().state, ST_PENDING_ENTER);
        // No A tick in HOUR0+1 ⇒ at HOUR0+2 A's bucket is empty, the z is
        // absent, nothing exits — and the unfilled entry is CARRIED (the
        // research fills at the next executable bar).
        s.on_tick(&tick(B, mono_at(HOUR0 + 1, 7), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 2, 1), &mut c);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.state, ST_PENDING_ENTER);
        assert_eq!(tv.intent, INTENT_ENTER);
        assert_eq!(tv.entry_hour, HOUR0 + 1, "max-hold still counts from the decision");
        assert_eq!(s.counters().intents_carried, 1);
        assert!(c.orders.is_empty());
        // The first fresh A tick two hours later prices it.
        s.on_tick(&tick(A, mono_at(HOUR0 + 2, 10), 104_000_000, false), &mut c);
        assert_eq!(c.orders.len(), 1);
        assert_eq!(c.orders[0].px.raw(), 104_000_000);
        assert_eq!(s.target_view(0).unwrap().state, ST_ENTERED);
        // Cancelled-by-exit: a fresh A tick would fill a pending entry, so
        // the only exit that can reach an UNFILLED one is the regime
        // hard-close (a signal exit needs A's close, which needs a fresh
        // tick). Flatten, re-dislocate, then hard-close the new entry.
        let hard = RegimeGate::new([core_types::RegimeWord::UNKNOWN; 4], false, core_types::REGIME_OFF_HARD);
        s.on_regime(hard, &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0 + 2, 11), 104_000_000, false), &mut c);
        assert_eq!(c.orders.len(), 2, "the hard-close exit priced");
        assert_eq!(s.counters().exits_regime, 1);
        s.on_regime(RegimeGate::OPEN_UNKNOWN, &mut c);
        // Cooldown from the exit FILL hour (+2): +3 is blocked, +4 may enter.
        s.on_tick(&tick(A, mono_at(HOUR0 + 2, 12), 110_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0 + 2, 13), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 3, 1), &mut c);
        assert_eq!(s.target_view(0).unwrap().state, ST_FLAT, "cooldown from the fill hour");
        s.on_tick(&tick(A, mono_at(HOUR0 + 3, 10), 110_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0 + 3, 11), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 4, 1), &mut c);
        assert_eq!(s.target_view(0).unwrap().state, ST_PENDING_ENTER);
        s.on_regime(hard, &mut c);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.state, ST_FLAT);
        assert_eq!(s.counters().entries_cancelled, 1);
        assert_eq!(c.orders.len(), 2, "a cancel prices nothing");
    }

    #[test]
    fn regime_soft_blocks_entries_and_hard_flattens() {
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), &mut c);
        let soft = RegimeGate::new([core_types::RegimeWord::UNKNOWN; 4], false, core_types::REGIME_OFF_SOFT);
        s.on_regime(soft, &mut c);
        s.on_timer(mono_at(HOUR0 + 1, 1), &mut c);
        assert_eq!(s.target_view(0).unwrap().state, ST_FLAT);
        assert_eq!(s.counters().regime_blocked, 1);
        // Reopen, enter, then hard-close: the position is armed to exit.
        s.on_regime(RegimeGate::OPEN_UNKNOWN, &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0 + 1, 11), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 2, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0 + 2, 6), 105_000_000, false), &mut c);
        assert_eq!(s.target_view(0).unwrap().state, ST_ENTERED);
        let hard = RegimeGate::new([core_types::RegimeWord::UNKNOWN; 4], false, core_types::REGIME_OFF_HARD);
        s.on_regime(hard, &mut c);
        let tv = s.target_view(0).unwrap();
        assert_eq!(tv.intent, INTENT_EXIT);
        assert_eq!(tv.exit_reason, EXIT_REGIME);
        s.on_tick(&tick(A, mono_at(HOUR0 + 2, 7), 105_000_000, false), &mut c);
        assert_eq!(s.counters().exits_regime, 1);
        assert_eq!(s.positions(), 0);
    }

    #[test]
    fn caps_refuse_and_count_without_crashing() {
        let mut p = params();
        p.max_positions = 1;
        let mut s = XsdStrategy::new();
        s.configure(anchor(), &p, &table(&[(A, B, SCALE_1E9), (C, B, SCALE_1E9)]))
            .unwrap();
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        // C mirrors A's wobble so both dislocate together.
        let mut h = HOUR0 - SEED_H;
        let mut k = 0i64;
        while h < HOUR0 {
            let a = if k % 2 == 0 { 100_000_000 } else { 100_200_000 };
            assert!(s.seed_close(C, h, a));
            h += 1;
            k += 1;
        }
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(C, mono_at(HOUR0, 11), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 12), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 1, 1), &mut c);
        assert_eq!(s.target_view(0).unwrap().state, ST_PENDING_ENTER);
        assert_eq!(s.target_view(1).unwrap().state, ST_PENDING_ENTER);
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 6), 105_000_000, false), &mut c);
        s.on_tick(&tick(C, mono_at(HOUR0 + 1, 7), 105_000_000, false), &mut c);
        assert_eq!(c.orders.len(), 1, "max_positions = 1 refuses the second");
        assert_eq!(s.counters().caps_rejected, 1);
        assert_eq!(s.target_view(1).unwrap().state, ST_FLAT);
        assert_eq!(s.positions(), 1);
    }

    #[test]
    fn ring_full_keeps_the_intent_pending() {
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 1, 1), &mut c);
        c.full = true;
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 6), 105_000_000, false), &mut c);
        assert_eq!(s.orders_dropped(), 1);
        assert_eq!(s.target_view(0).unwrap().intent, INTENT_ENTER);
        c.full = false;
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 7), 105_000_000, false), &mut c);
        assert_eq!(c.orders.len(), 1);
        assert_eq!(s.target_view(0).unwrap().state, ST_ENTERED);
    }

    #[test]
    fn a_tick_past_the_boundary_rolls_before_it_is_booked() {
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, false), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), &mut c);
        // No timer at HOUR0+1; the first tick past it must close HOUR0 on
        // the 105 print (not on itself) and then price the entry.
        s.on_tick(&tick(A, mono_at(HOUR0 + 1, 40), 100_000_000, false), &mut c);
        assert_eq!(s.current_hour(), HOUR0 + 1);
        let tv = s.target_view(0).unwrap();
        assert!(tv.zbar_1e9 > 3 * SCALE_1E9, "the closed hour's mid was the 105 print");
        assert_eq!(c.orders.len(), 1);
        assert_eq!(c.orders[0].px.raw(), 100_000_000, "priced at the new tick");
    }

    #[test]
    fn a_stale_close_leaves_the_bucket_empty() {
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        let mut c = ctx();
        seed_wobble(&mut s, HOUR0 - 1, SEED_H);
        s.on_timer(mono_at(HOUR0, 1), &mut c);
        s.on_tick(&tick(A, mono_at(HOUR0, 10), 105_000_000, true), &mut c);
        s.on_tick(&tick(B, mono_at(HOUR0, 11), 100_000_000, false), &mut c);
        s.on_timer(mono_at(HOUR0 + 1, 1), &mut c);
        let pv = s.pair_view(0, 0).unwrap();
        assert!(!pv.z_valid);
        assert_eq!(pv.n, SEED_H as u32, "the closed hour is missing on A");
        assert_eq!(s.target_view(0).unwrap().state, ST_FLAT);
    }

    // ---- restore ----

    #[test]
    fn restore_resumes_or_flattens_by_flag() {
        let mut s = configured(&[(A, B, SCALE_1E9)]);
        let row = XsdRestoreRow {
            sym: A,
            side: SIDE_LONG,
            d: 1,
            grid_units: 1,
            qty_1e6: 9_500_000,
            notional_1e6: 1_000_000_000,
            entry_hour: HOUR0 - 3,
            last_add_hour: HOUR0 - 3,
        };
        assert!(s.restore_position(&row, false).is_ok());
        assert_eq!(s.positions(), 1);
        assert_eq!(s.open_notional_total_1e6(), 1_000_000_000);
        let mut out = [XsdPositionView::default(); 4];
        assert_eq!(s.positions_view(&mut out), 1);
        assert_eq!(out[0].sym, A);
        assert_eq!(out[0].entry_hour, HOUR0 - 3);
        assert_eq!(s.xsd_positions_view(&mut out), 1);
        assert_eq!(s.xsd_state_epoch(), 1);
        // Duplicate refused; partner-only and outsider refused.
        assert!(s.restore_position(&row, false).is_err());
        let mut b = row;
        b.sym = B;
        assert!(s.restore_position(&b, false).is_err());
        let mut z = row;
        z.sym = make_symbol_id(VenueId::Okx, 77);
        assert!(s.restore_position(&z, false).is_err());
        let mut bad = row;
        bad.side = 3;
        bad.sym = A;
        // (A already holds a position — use a fresh member for the shape checks.)
        let mut s2 = configured(&[(A, B, SCALE_1E9)]);
        assert!(s2.restore_position(&bad, false).is_err());
        bad.side = SIDE_LONG;
        bad.qty_1e6 = 0;
        assert!(s2.restore_position(&bad, false).is_err());
        // Flatten: armed to EXIT rotation at the first fresh tick.
        assert!(s2.restore_position(&row, true).is_ok());
        let mut c = ctx();
        s2.on_tick(&tick(A, mono_at(HOUR0, 5), 100_000_000, false), &mut c);
        assert_eq!(c.orders.len(), 1);
        assert_eq!(c.orders[0].side, Side::Ask);
        assert_eq!(c.orders[0].qty.raw(), 9_500_000);
        assert_eq!(s2.counters().exits_rotation, 1);
        assert_eq!(s2.positions(), 0);
        // Restore before configure refused.
        let mut s3 = XsdStrategy::new();
        assert!(s3.restore_position(&row, false).is_err());
    }

    #[test]
    fn strategy_counters_surface() {
        let s = configured(&[(A, B, SCALE_1E9)]);
        assert_eq!(s.strategy_kind(), "xsd");
        assert_eq!(s.orders_emitted(), 0);
        assert_eq!(s.xsd_counters(), XsdCounters::default());
        assert_eq!(s.regime_label(), RegimeLabelSet::ANY);
    }
}
