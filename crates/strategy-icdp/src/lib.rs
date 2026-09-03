// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-icdp — intrabar candle-direction prediction (slot 6)
//!
//! The first tenant of the intrabar substrate (ICDP×VT plan I3): a
//! UTC-aligned bar grid over the monotonic clock
//! ([`core_time::BarClock`]), per-instrument L1 features computed from
//! every [`Tick`] (queue imbalance, microprice, Cont–Kukanov–Stoikov
//! order-flow imbalance, the early return, the previous bar's return),
//! a standardised linear composite fitted OFFLINE (the research vault's
//! arrays → `~/multivenue/icdp.toml`, a data artifact hashed into the
//! boot log), a confidence threshold, and **IoC taker intents**
//! (`Order.kind = 1`, `ttl_ns` = the bar's remaining life) — entry at
//! the decision point `open + δ`, exit at the bar roll. Exactly one
//! logical position per instrument per bar.
//!
//! ## Staleness (VT doctrine 3 as the strategy's skip rule)
//!
//! A stale tick (`TICK_FLAG_STALE`) updates nothing but the last-quote
//! fields and the `stale_in_bar` flag. The bar open is the last FRESH
//! quote before the boundary (a stale open skips the bar); the decision
//! fires on the first FRESH tick at/after `open + δ` and only if no
//! stale tick was seen in the bar so far; the exit at the roll is
//! emitted regardless (the harness defers its fill to the next fresh
//! tick — VT4/I1). The Binance-spot sentinel bit (bit1) is fresh.
//!
//! ## Hot-path rules
//!
//! Zero allocation after [`IcdpStrategy::configure`]; no floats — every
//! feature is an `i64` ×1e9 with `i128` intermediates and FLOOR
//! division (the Python reference in the vault uses the same integer
//! law, so decisions are bit-identical); one fixed `[IcdpSym; 32]`
//! table resolved by binary search over a sorted sym list; no `dyn`;
//! `debug_assert!` on every invariant (release = abort). The only
//! division per tick is the feature math AT the decision; the bar roll
//! is a compare against a precomputed close.

#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc
)]

use core_time::{BarClock, NsTs, WallAnchor};
use core_types::{
    symbol_venue_byte, Order, Price, Qty, Side, SymbolId, Tick, VenueId, SYMBOL_ID_NONE,
};
use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

/// The counters the engine mirrors (defined in `strategy-core` so the
/// cli never names this crate — the vm-gauge precedent).
pub use strategy_core::IcdpCounters;

/// Instrument capacity (D4: 8 majors in v1; the table is sized for the
/// research universe).
pub const ICDP_MAX_SYMS: usize = 32;
/// Composite feature count: `r_early, imb, micro, ofi, r_prev` (the
/// research note's composite minus `tflow` — no trade lane reaches the
/// strategy in v1 — and minus `n_early`/`spread`/`rng_prev`, which the
/// fit excludes).
pub const ICDP_NF: usize = 5;
/// Fixed-point scale of every feature, weight, threshold (×1e9).
pub const SCALE_1E9: i64 = 1_000_000_000;
/// `Order.kind` of the IoC intents this strategy emits (I1 law).
pub const ORDER_KIND_IOC: u8 = 1;
/// Last-fifth-of-the-bar law: a decision tick arriving at/after
/// `open + 4/5 tf` is LATE — the bar is skipped (quiet instrument).
const LATE_NUM: u64 = 4;
const LATE_DEN: u64 = 5;
/// Quiet-instrument sweep cadence (ticks of ANY sym).
const SWEEP_EVERY: u32 = 256;
/// Risk-policy caps mirrored (docs/risk-policy.md): per leg, per sym,
/// per table — USD ×1e6.
pub const CAP_LEG_1E6: i64 = 10_000_000_000;
/// Per-instrument cap USD ×1e6.
pub const CAP_SYM_1E6: i64 = 20_000_000_000;
/// Table cap USD ×1e6.
pub const CAP_TABLE_1E6: i64 = 100_000_000_000;

const POS_NONE: u8 = 0;
const POS_LONG: u8 = 1;
const POS_SHORT: u8 = 2;

/// Per-instrument fitted parameters (×1e9 fixed point) — one
/// `[[instrument]]` block of `icdp.toml`. POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct IcdpSymParams {
    /// Resolved namespaced sym.
    pub sym: SymbolId,
    /// Feature means (×1e9), research order.
    pub mu: [i64; ICDP_NF],
    /// Inverse feature sds (×1e9) — `1e9 / sd` precomputed offline so
    /// standardisation is one multiply.
    pub inv_sd: [i64; ICDP_NF],
    /// Ridge weights (×1e9).
    pub w: [i64; ICDP_NF],
    /// Intercept (×1e9).
    pub b: i64,
    /// Fire when `|s| > thr` (×1e9).
    pub thr: i64,
    /// Clip notional per position, USD ×1e6 (≤ [`CAP_LEG_1E6`]).
    pub notional_1e6: i64,
    /// Skip when the spread at decision exceeds this (bps ×1e9).
    pub spread_cap_1e9: i64,
    /// Entry limit = touch ± this (bps ×1e9).
    pub entry_slip_1e9: i64,
    /// Exit limit = touch ∓ this (bps ×1e9).
    pub exit_slip_1e9: i64,
}

impl IcdpSymParams {
    /// An empty (unused) slot.
    pub const EMPTY: Self = Self {
        sym: SYMBOL_ID_NONE,
        mu: [0; ICDP_NF],
        inv_sd: [0; ICDP_NF],
        w: [0; ICDP_NF],
        b: 0,
        thr: 0,
        notional_1e6: 0,
        spread_cap_1e9: 0,
        entry_slip_1e9: 0,
        exit_slip_1e9: 0,
    };
}

/// The whole artifact: grid + instruments + identity. POD, built by the
/// cli from `icdp.toml` (allocation there, never here).
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct IcdpParams {
    /// Bar length ns.
    pub tf_ns: u64,
    /// Decision offset ns (`< tf_ns`).
    pub delta_ns: u64,
    /// Instruments in use (`≤ ICDP_MAX_SYMS`).
    pub n: usize,
    /// Per-instrument blocks (`..n` meaningful).
    pub syms: [IcdpSymParams; ICDP_MAX_SYMS],
    /// SHA-256 of the artifact file — logged at boot and stamped into
    /// the audit-pnl header (the ruleset-hash precedent).
    pub hash: [u8; 32],
}

impl IcdpParams {
    /// An empty artifact (nothing configured).
    pub const EMPTY: Self = Self {
        tf_ns: 0,
        delta_ns: 0,
        n: 0,
        syms: [IcdpSymParams::EMPTY; ICDP_MAX_SYMS],
        hash: [0; 32],
    };
}

/// One instrument's live state — two cache lines.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
struct IcdpSym {
    // --- identity / params (read-only after configure) ---
    p: IcdpSymParams,
    venue_byte: u8,
    // --- last quote ---
    last_stale: u8,
    pos_side: u8,
    open_stale: u8,
    stale_in_bar: u8,
    dec_done: u8,
    prev_valid: u8,
    open_valid: u8,
    bid: i64,
    ask: i64,
    bq: i64,
    aq: i64,
    last_ts: NsTs,
    // --- bar state ---
    bar_id: u64,
    close_mono: NsTs,
    decision_mono: NsTs,
    late_mono: NsTs,
    open_mid: i64,
    tot_open: i64,
    ofi_acc: i64,
    prev_open_mid: i64,
    // --- logical position ---
    pos_qty: i64,
    pos_notional_1e6: i64,
}

impl IcdpSym {
    const EMPTY: Self = Self {
        p: IcdpSymParams::EMPTY,
        venue_byte: 0,
        last_stale: 0,
        pos_side: POS_NONE,
        open_stale: 0,
        stale_in_bar: 0,
        dec_done: 0,
        prev_valid: 0,
        open_valid: 0,
        bid: 0,
        ask: 0,
        bq: 0,
        aq: 0,
        last_ts: 0,
        bar_id: 0,
        close_mono: u64::MAX,
        decision_mono: u64::MAX,
        late_mono: u64::MAX,
        open_mid: 0,
        tot_open: 0,
        ofi_acc: 0,
        prev_open_mid: 0,
        pos_qty: 0,
        pos_notional_1e6: 0,
    };
}

/// The strategy. See the module docs.
#[repr(C, align(64))]
pub struct IcdpStrategy {
    clock: BarClock,
    syms: [IcdpSym; ICDP_MAX_SYMS],
    /// Sorted sym ids (`..n`) for the per-tick binary search.
    index: [SymbolId; ICDP_MAX_SYMS],
    /// Slot of each sorted entry.
    slot_of: [u8; ICDP_MAX_SYMS],
    n: usize,
    configured: bool,
    hash: [u8; 32],
    counters: IcdpCounters,
    open_notional_total_1e6: i64,
    orders_emitted: u64,
    orders_dropped: u64,
    next_oid: u64,
    tick_count: u32,
}

impl Default for IcdpStrategy {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Feature math (integer, floor division — mirrored in the vault's
// Python reference; every helper is pub so the golden test and the
// research port can pin them one by one).
// ---------------------------------------------------------------

/// Integer mid ×1e6 (floor of the sum halved — the reference uses the
/// same).
#[inline(always)]
pub const fn mid_1e6(bid: i64, ask: i64) -> i64 {
    (bid + ask) >> 1
}

#[inline(always)]
const fn floor_div(n: i128, d: i128) -> i128 {
    n.div_euclid(d)
}

/// Return of `to` over `from` in bps ×1e9: `((to − from) × 1e13) / from`.
#[inline(always)]
pub fn ret_bps_1e9(from_1e6: i64, to_1e6: i64) -> i64 {
    debug_assert!(from_1e6 > 0);
    floor_div(
        (to_1e6 as i128 - from_1e6 as i128) * 10_000_000_000_000,
        from_1e6 as i128,
    ) as i64
}

/// L1 queue imbalance ×1e9: `(bq − aq) / max(bq + aq, 1)`.
#[inline(always)]
pub fn imb_1e9(bq: i64, aq: i64) -> i64 {
    let tot = (bq as i128 + aq as i128).max(1);
    floor_div((bq as i128 - aq as i128) * SCALE_1E9 as i128, tot) as i64
}

/// Microprice minus mid in bps ×1e9:
/// `((bid·aq + ask·bq) − mid·(bq+aq)) × 1e13 / (mid·(bq+aq))`.
#[inline(always)]
pub fn micro_bps_1e9(bid: i64, ask: i64, bq: i64, aq: i64) -> i64 {
    let mid = mid_1e6(bid, ask) as i128;
    let tot = (bq as i128 + aq as i128).max(1);
    let num = bid as i128 * aq as i128 + ask as i128 * bq as i128 - mid * tot;
    floor_div(num * 10_000_000_000_000, (mid * tot).max(1)) as i64
}

/// One Cont–Kukanov–Stoikov OFI increment between two consecutive
/// quotes (qty ×1e6 units).
#[inline(always)]
pub const fn ofi_step(pb: i64, pbq: i64, pa: i64, paq: i64, b: i64, bq: i64, a: i64, aq: i64) -> i64 {
    let db = b - pb;
    let da = a - pa;
    let mut e = 0i64;
    if db >= 0 {
        e += bq;
    }
    if db <= 0 {
        e -= pbq;
    }
    if da <= 0 {
        e -= aq;
    }
    if da >= 0 {
        e += paq;
    }
    e
}

/// OFI normalised by the mean L1 depth of open and decision ×1e9:
/// `ofi / max(0.5·(tot_open + tot_dec), 1.0)`.
#[inline(always)]
pub fn ofi_norm_1e9(ofi_acc: i64, tot_open: i64, tot_dec: i64) -> i64 {
    let den2 = (tot_open as i128 + tot_dec as i128).max(2_000_000);
    floor_div(ofi_acc as i128 * 2 * SCALE_1E9 as i128, den2) as i64
}

/// Spread in bps ×1e9: `(ask − bid) × 1e13 / mid`.
#[inline(always)]
pub fn spread_bps_1e9(bid: i64, ask: i64) -> i64 {
    let mid = (mid_1e6(bid, ask) as i128).max(1);
    floor_div((ask as i128 - bid as i128) * 10_000_000_000_000, mid) as i64
}

/// The composite: `b + Σ w_i · ((f_i − mu_i) · inv_sd_i / 1e9) / 1e9` (×1e9).
#[inline(always)]
pub fn composite_1e9(p: &IcdpSymParams, f: &[i64; ICDP_NF]) -> i64 {
    let mut s = p.b as i128;
    let mut i = 0usize;
    while i < ICDP_NF {
        let z = floor_div((f[i] as i128 - p.mu[i] as i128) * p.inv_sd[i] as i128, SCALE_1E9 as i128);
        s += floor_div(p.w[i] as i128 * z, SCALE_1E9 as i128);
        i += 1;
    }
    s as i64
}

/// Price shifted by `bps_1e9` basis points (×1e9): `px ± px·bps/1e13`.
#[inline(always)]
pub fn shift_px_1e6(px_1e6: i64, bps_1e9: i64, up: bool) -> i64 {
    let d = floor_div(px_1e6 as i128 * bps_1e9 as i128, 10_000_000_000_000) as i64;
    if up {
        px_1e6 + d
    } else {
        (px_1e6 - d).max(1)
    }
}

/// Quantity ×1e6 for `notional_1e6` USD at `px_1e6` (floor).
#[inline(always)]
pub fn qty_for_notional_1e6(notional_1e6: i64, px_1e6: i64) -> i64 {
    debug_assert!(px_1e6 > 0);
    floor_div(notional_1e6 as i128 * 1_000_000, px_1e6 as i128) as i64
}

impl IcdpStrategy {
    /// Unconfigured strategy (inert until [`Self::configure`]).
    pub const fn new() -> Self {
        Self {
            clock: BarClock {
                anchor: WallAnchor { mono_ns: 0, wall_ns: 0 },
                tf_ns: 1,
                delta_ns: 0,
            },
            syms: [IcdpSym::EMPTY; ICDP_MAX_SYMS],
            index: [SYMBOL_ID_NONE; ICDP_MAX_SYMS],
            slot_of: [0; ICDP_MAX_SYMS],
            n: 0,
            configured: false,
            hash: [0; 32],
            counters: IcdpCounters {
                decisions: 0,
                signals: 0,
                intents: 0,
                exits: 0,
                exit_on_stale: 0,
                skipped_spread: 0,
                skipped_stale_open: 0,
                skipped_stale_dec: 0,
                skipped_prev: 0,
                late_bars: 0,
                caps_rejected: 0,
                rolls: 0,
            },
            open_notional_total_1e6: 0,
            orders_emitted: 0,
            orders_dropped: 0,
            next_oid: 1,
            tick_count: 0,
        }
    }

    /// Boot-time configuration: the wall anchor, the artifact. Fails
    /// closed on anything the caps or the grid refuse; a refused
    /// config leaves the strategy unconfigured (`on_start` then errs
    /// when the slot is enabled).
    pub fn configure(&mut self, anchor: WallAnchor, params: &IcdpParams) -> Result<(), StrategyError> {
        if params.tf_ns == 0 || params.delta_ns >= params.tf_ns {
            return Err(StrategyError::Config("icdp: delta must fall inside a positive tf"));
        }
        if params.n == 0 || params.n > ICDP_MAX_SYMS {
            return Err(StrategyError::Config("icdp: instrument count out of range"));
        }
        // Validate every block before touching state (all-or-nothing).
        let mut i = 0usize;
        while i < params.n {
            let p = &params.syms[i];
            if p.sym == SYMBOL_ID_NONE {
                return Err(StrategyError::Config("icdp: unresolved instrument"));
            }
            if p.notional_1e6 <= 0 || p.notional_1e6 > CAP_LEG_1E6 {
                return Err(StrategyError::Config("icdp: notional outside (0, $10k]"));
            }
            if p.thr <= 0 {
                return Err(StrategyError::Config("icdp: threshold must be positive"));
            }
            if p.spread_cap_1e9 <= 0 || p.entry_slip_1e9 < 0 || p.exit_slip_1e9 < 0 {
                return Err(StrategyError::Config("icdp: spread cap / slips malformed"));
            }
            let mut k = 0usize;
            while k < ICDP_NF {
                if p.inv_sd[k] <= 0 {
                    return Err(StrategyError::Config("icdp: inv_sd must be positive"));
                }
                k += 1;
            }
            let mut j = 0usize;
            while j < i {
                if params.syms[j].sym == p.sym {
                    return Err(StrategyError::Config("icdp: duplicate instrument"));
                }
                j += 1;
            }
            i += 1;
        }
        self.clock = BarClock::new(anchor, params.tf_ns, params.delta_ns);
        self.syms = [IcdpSym::EMPTY; ICDP_MAX_SYMS];
        let mut i = 0usize;
        while i < params.n {
            let p = params.syms[i];
            self.syms[i].p = p;
            self.syms[i].venue_byte = symbol_venue_byte(p.sym);
            self.index[i] = p.sym;
            self.slot_of[i] = i as u8;
            i += 1;
        }
        // Insertion sort of (index, slot_of) by sym — ≤ 32 entries, boot only.
        let mut a = 1usize;
        while a < params.n {
            let mut b = a;
            while b > 0 && self.index[b - 1] > self.index[b] {
                self.index.swap(b - 1, b);
                self.slot_of.swap(b - 1, b);
                b -= 1;
            }
            a += 1;
        }
        self.n = params.n;
        self.hash = params.hash;
        self.open_notional_total_1e6 = 0;
        self.configured = true;
        Ok(())
    }

    /// True after a successful [`Self::configure`].
    #[inline]
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// Artifact hash (boot log / report header).
    #[inline]
    pub fn params_hash(&self) -> &[u8; 32] {
        &self.hash
    }

    /// Instruments configured.
    #[inline]
    pub fn instruments(&self) -> usize {
        self.n
    }

    /// The bar grid in use.
    #[inline]
    pub fn clock(&self) -> &BarClock {
        &self.clock
    }

    /// Diagnostic counters.
    #[inline]
    pub fn counters(&self) -> &IcdpCounters {
        &self.counters
    }

    /// Logical open notional across the table, USD ×1e6.
    #[inline]
    pub fn open_notional_total_1e6(&self) -> i64 {
        self.open_notional_total_1e6
    }

    /// Logical position side of a configured sym (0 none, 1 long,
    /// 2 short) — test/dashboard surface.
    pub fn position_side(&self, sym: SymbolId) -> u8 {
        match self.slot(sym) {
            Some(i) => self.syms[i].pos_side,
            None => POS_NONE,
        }
    }

    /// Binary search over the sorted sym list.
    #[inline(always)]
    fn slot(&self, sym: SymbolId) -> Option<usize> {
        let mut lo = 0usize;
        let mut hi = self.n;
        while lo < hi {
            let mid = lo + ((hi - lo) >> 1);
            let v = self.index[mid];
            if v == sym {
                return Some(self.slot_of[mid] as usize);
            }
            if v < sym {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        None
    }

    #[inline(always)]
    fn emit<C: Ctx>(&mut self, ctx: &mut C, i: usize, side: Side, px: i64, qty: i64, ttl: u64, now: NsTs) -> bool {
        debug_assert!(px > 0 && qty > 0);
        let s = &self.syms[i];
        // The wire venue byte is the sym-namespace law (the fill model
        // corrects the M1 anchor id 7 itself): VenueId::from_u8 of a
        // configured sym's byte cannot fail — bytes come from
        // `symbol_venue_byte` of a resolved id.
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
            s.p.sym,
            side,
            ORDER_KIND_IOC,
            Price::from_raw(px),
            Qty::from_raw(qty),
            self.next_oid,
        )
        .with_ttl_ns(ttl);
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

    /// Bar roll for slot `i` at `now` (≥ the slot's close): exit the
    /// open position, close the ended bar's stats, open the new bar on
    /// the prevailing quote.
    fn roll<C: Ctx>(&mut self, ctx: &mut C, i: usize, now: NsTs) {
        self.counters.rolls = self.counters.rolls.wrapping_add(1);
        let new_id = self.clock.bar_id(now);
        let s = self.syms[i];
        let ended_id = s.bar_id;
        let contiguous = s.open_valid != 0 && new_id == ended_id.wrapping_add(1);
        // ---- exit at the roll (I3 rule: emitted regardless of staleness) ----
        if s.pos_side != POS_NONE && s.bid > 0 && s.ask > 0 {
            let (side, px) = if s.pos_side == POS_LONG {
                (Side::Ask, shift_px_1e6(s.bid, s.p.exit_slip_1e9, false))
            } else {
                (Side::Bid, shift_px_1e6(s.ask, s.p.exit_slip_1e9, true))
            };
            let ttl = self.clock.ttl_to_close(new_id, now);
            if self.emit(ctx, i, side, px, s.pos_qty, ttl, now) {
                self.counters.exits = self.counters.exits.wrapping_add(1);
                if s.last_stale != 0 {
                    self.counters.exit_on_stale = self.counters.exit_on_stale.wrapping_add(1);
                }
            }
            self.open_notional_total_1e6 -= s.pos_notional_1e6;
            debug_assert!(self.open_notional_total_1e6 >= 0);
        }
        let s = &mut self.syms[i];
        s.pos_side = POS_NONE;
        s.pos_qty = 0;
        s.pos_notional_1e6 = 0;
        // ---- previous-bar stats (only a contiguous, clean bar counts) ----
        if contiguous && s.open_stale == 0 && s.stale_in_bar == 0 && s.open_mid > 0 {
            s.prev_open_mid = s.open_mid;
            s.prev_valid = 1;
        } else {
            s.prev_valid = 0;
        }
        // ---- new bar on the prevailing quote (the last quote before the boundary) ----
        // The research's validity law: the boundary quote is at most
        // max(tf, 2 s) old at the boundary; older ⇒ the open is unknown.
        let open_mono = self.clock.open_mono(new_id);
        let max_age = if self.clock.tf_ns > 2_000_000_000 { self.clock.tf_ns } else { 2_000_000_000 };
        let quote_age = open_mono.saturating_sub(s.last_ts);
        let fresh_quote = s.last_stale == 0 && s.bid > 0 && s.ask > 0 && quote_age <= max_age;
        s.bar_id = new_id;
        s.close_mono = self.clock.close_mono(new_id);
        s.decision_mono = self.clock.decision_mono(new_id);
        s.late_mono = open_mono.wrapping_add(self.clock.tf_ns / LATE_DEN * LATE_NUM);
        s.dec_done = 0;
        s.stale_in_bar = 0;
        s.ofi_acc = 0;
        if fresh_quote {
            s.open_mid = mid_1e6(s.bid, s.ask);
            s.tot_open = s.bq + s.aq;
            s.open_stale = 0;
            s.open_valid = 1;
        } else {
            s.open_mid = 0;
            s.tot_open = 0;
            s.open_stale = 1;
            s.open_valid = 1; // the bar exists (ids stay contiguous); its open is stale
        }
    }

    /// The decision at the first fresh tick at/after `open + δ`.
    fn decide<C: Ctx>(&mut self, ctx: &mut C, i: usize, now: NsTs) {
        let s = self.syms[i];
        self.syms[i].dec_done = 1;
        self.counters.decisions = self.counters.decisions.wrapping_add(1);
        if now >= s.late_mono {
            self.counters.late_bars = self.counters.late_bars.wrapping_add(1);
            return;
        }
        if s.open_stale != 0 || s.open_mid <= 0 {
            self.counters.skipped_stale_open = self.counters.skipped_stale_open.wrapping_add(1);
            return;
        }
        if s.stale_in_bar != 0 {
            self.counters.skipped_stale_dec = self.counters.skipped_stale_dec.wrapping_add(1);
            return;
        }
        if s.prev_valid == 0 || s.prev_open_mid <= 0 {
            self.counters.skipped_prev = self.counters.skipped_prev.wrapping_add(1);
            return;
        }
        if spread_bps_1e9(s.bid, s.ask) > s.p.spread_cap_1e9 {
            self.counters.skipped_spread = self.counters.skipped_spread.wrapping_add(1);
            return;
        }
        let mid = mid_1e6(s.bid, s.ask);
        let f: [i64; ICDP_NF] = [
            ret_bps_1e9(s.open_mid, mid),
            imb_1e9(s.bq, s.aq),
            micro_bps_1e9(s.bid, s.ask, s.bq, s.aq),
            ofi_norm_1e9(s.ofi_acc, s.tot_open, s.bq + s.aq),
            ret_bps_1e9(s.prev_open_mid, s.open_mid),
        ];
        let sc = composite_1e9(&s.p, &f);
        if sc.abs() <= s.p.thr {
            return;
        }
        self.counters.signals = self.counters.signals.wrapping_add(1);
        if self.open_notional_total_1e6 + s.p.notional_1e6 > CAP_TABLE_1E6 {
            self.counters.caps_rejected = self.counters.caps_rejected.wrapping_add(1);
            return;
        }
        let long = sc > 0;
        let (side, px) = if long {
            (Side::Bid, shift_px_1e6(s.ask, s.p.entry_slip_1e9, true))
        } else {
            (Side::Ask, shift_px_1e6(s.bid, s.p.entry_slip_1e9, false))
        };
        let qty = qty_for_notional_1e6(s.p.notional_1e6, px);
        if qty <= 0 {
            return;
        }
        let ttl = self.clock.ttl_to_close(s.bar_id, now);
        if self.emit(ctx, i, side, px, qty, ttl, now) {
            self.counters.intents = self.counters.intents.wrapping_add(1);
            let s = &mut self.syms[i];
            s.pos_side = if long { POS_LONG } else { POS_SHORT };
            s.pos_qty = qty;
            s.pos_notional_1e6 = s.p.notional_1e6;
            self.open_notional_total_1e6 += s.p.notional_1e6;
        }
    }

    /// Quiet-instrument sweep: roll every slot whose bar has closed
    /// (their own ticks stopped coming) so exits leave on time.
    fn sweep<C: Ctx>(&mut self, ctx: &mut C, now: NsTs) {
        let mut i = 0usize;
        while i < self.n {
            if self.syms[i].open_valid != 0 && now >= self.syms[i].close_mono {
                self.roll(ctx, i, now);
            }
            i += 1;
        }
    }
}

impl StrategyCounters for IcdpStrategy {
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
        "icdp"
    }
    #[inline]
    fn icdp_counters(&self) -> IcdpCounters {
        self.counters
    }
}

impl Strategy for IcdpStrategy {
    /// Enabled without an artifact ⇒ refuse the boot (the cli only
    /// sets the bit when `icdp.toml` resolved; belt and braces).
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if !self.configured {
            return Err(StrategyError::Config("icdp: enabled without a resolved icdp.toml"));
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        let now = tick.ts_ns;
        self.tick_count = self.tick_count.wrapping_add(1);
        if self.tick_count % SWEEP_EVERY == 0 {
            self.sweep(ctx, now);
        }
        let i = match self.slot(tick.sym) {
            Some(i) => i,
            None => return,
        };
        let bid = tick.bid_px.raw();
        let ask = tick.ask_px.raw();
        let stale = tick.is_stale();
        // ---- first sighting: open the bar containing this tick ----
        if self.syms[i].open_valid == 0 {
            let s = &mut self.syms[i];
            s.bid = bid;
            s.ask = ask;
            s.bq = tick.bid_qty.raw();
            s.aq = tick.ask_qty.raw();
            s.last_ts = now;
            s.last_stale = stale as u8;
            // Pretend the previous bar closed now: `roll` opens the
            // current bar on this quote with prev_valid = 0.
            s.bar_id = self.clock.bar_id(now).wrapping_sub(1);
            s.close_mono = now;
            self.roll(ctx, i, now);
            return;
        }
        if now >= self.syms[i].close_mono {
            self.roll(ctx, i, now);
        }
        let s = &mut self.syms[i];
        if stale || bid <= 0 || ask <= 0 {
            // Stale (or one-sided): last-quote fields + the flag only.
            s.bid = bid;
            s.ask = ask;
            s.bq = tick.bid_qty.raw();
            s.aq = tick.ask_qty.raw();
            s.last_ts = now;
            s.last_stale = 1;
            if stale {
                s.stale_in_bar = 1;
            }
            return;
        }
        let bq = tick.bid_qty.raw();
        let aq = tick.ask_qty.raw();
        if s.last_stale == 0 && s.bid > 0 && s.ask > 0 {
            s.ofi_acc = s
                .ofi_acc
                .wrapping_add(ofi_step(s.bid, s.bq, s.ask, s.aq, bid, bq, ask, aq));
        }
        s.bid = bid;
        s.ask = ask;
        s.bq = bq;
        s.aq = aq;
        s.last_ts = now;
        s.last_stale = 0;
        if s.dec_done == 0 && now >= s.decision_mono {
            self.decide(ctx, i, now);
        }
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, _signal: &core_types::Signal, _ctx: &mut C) {}

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, _fill: &core_types::Fill, _ctx: &mut C) {}

    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}

    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, TICK_FLAG_STALE};

    const MS: u64 = 1_000_000;
    const TF: u64 = 15_000 * MS;
    const DELTA: u64 = 3_750 * MS;
    const WALL: u64 = 1_788_400_000_000_000_000;
    const MONO: u64 = 3_191_000_000_000_000;

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

    const BN: SymbolId = 7; // the M1 anchor (venue byte 0 on the wire)
    const OKX: SymbolId = make_symbol_id(VenueId::Okx, 1);

    fn params(syms: &[SymbolId], thr: i64) -> IcdpParams {
        let mut p = IcdpParams::EMPTY;
        p.tf_ns = TF;
        p.delta_ns = DELTA;
        p.n = syms.len();
        for (i, s) in syms.iter().enumerate() {
            p.syms[i] = IcdpSymParams {
                sym: *s,
                mu: [0; ICDP_NF],
                inv_sd: [SCALE_1E9; ICDP_NF], // sd = 1 ⇒ z = f
                w: [SCALE_1E9, 0, 0, 0, 0],   // s = r_early (bps)
                b: 0,
                thr,
                notional_1e6: 10_000_000_000, // $10k
                spread_cap_1e9: 5 * SCALE_1E9, // 5 bps
                entry_slip_1e9: SCALE_1E9,     // 1 bps
                exit_slip_1e9: 2 * SCALE_1E9,  // 2 bps
            };
        }
        p.hash = [7; 32];
        p
    }

    fn strat(syms: &[SymbolId], thr: i64) -> IcdpStrategy {
        let mut s = IcdpStrategy::new();
        s.configure(WallAnchor::new(MONO, WALL), &params(syms, thr))
            .expect("config");
        s
    }

    fn tick(sym: SymbolId, ts: NsTs, bid: i64, bq: i64, ask: i64, aq: i64, flags: u8) -> Tick {
        let venue = VenueId::from_u8(symbol_venue_byte(sym)).expect("venue");
        Tick::new_stamped(
            ts,
            venue,
            sym,
            1,
            Price::from_raw(bid),
            Qty::from_raw(bq),
            Price::from_raw(ask),
            Qty::from_raw(aq),
            0,
            flags,
        )
    }

    /// Open of the bar AFTER the one containing MONO (a clean start).
    fn t0(s: &IcdpStrategy) -> NsTs {
        let id = s.clock().bar_id(MONO) + 1;
        s.clock().open_mono(id)
    }

    // ---------- config ----------

    #[test]
    fn configure_refuses_bad_grids_caps_and_duplicates() {
        let mut s = IcdpStrategy::new();
        let a = WallAnchor::new(MONO, WALL);
        let mut p = params(&[BN], SCALE_1E9);
        p.delta_ns = p.tf_ns;
        assert!(s.configure(a, &p).is_err());
        let mut p = params(&[BN], SCALE_1E9);
        p.syms[0].notional_1e6 = CAP_LEG_1E6 + 1;
        assert!(s.configure(a, &p).is_err());
        let p = params(&[BN, BN], SCALE_1E9);
        assert!(s.configure(a, &p).is_err());
        let mut p = params(&[BN], SCALE_1E9);
        p.syms[0].sym = SYMBOL_ID_NONE;
        assert!(s.configure(a, &p).is_err());
        assert!(!s.is_configured());
        assert!(s.on_start(&mut ctx()).is_err(), "enabled without an artifact refuses");
        assert!(s.configure(a, &params(&[OKX, BN], SCALE_1E9)).is_ok());
        assert!(s.on_start(&mut ctx()).is_ok());
        assert_eq!(s.instruments(), 2);
        assert_eq!(s.params_hash(), &[7; 32]);
        // sorted index resolves both
        assert_eq!(s.position_side(BN), 0);
        assert_eq!(s.position_side(OKX), 0);
        assert_eq!(s.strategy_kind(), "icdp");
    }

    // ---------- feature math ----------

    #[test]
    fn feature_math_matches_hand_computed_integers() {
        // bid 100.000000 ask 100.010000 ⇒ mid 100.005000; spread 1 bps
        assert_eq!(mid_1e6(100_000_000, 100_010_000), 100_005_000);
        // 10_000 × 1e13 / 100_005_000 = 999_950_002.5 ⇒ floor
        assert_eq!(spread_bps_1e9(100_000_000, 100_010_000), 999_950_002);
        // +0.5 bps return
        assert_eq!(ret_bps_1e9(100_000_000, 100_005_000), 500_000_000);
        // negative return FLOORS: −499_975_001.25 ⇒ −499_975_002
        assert_eq!(ret_bps_1e9(100_005_000, 100_000_000), -499_975_002);
        // imbalance 3 vs 1 ⇒ +0.5
        assert_eq!(imb_1e9(3_000_000, 1_000_000), 500_000_000);
        assert_eq!(imb_1e9(0, 0), 0);
        // microprice with bq 3, aq 1: (bid·1 + ask·3)/4 = 100.0075 ⇒ +0.25 bps − mid
        assert_eq!(micro_bps_1e9(100_000_000, 100_010_000, 3_000_000, 1_000_000), 249_987_500);
        // OFI: bid up with new bq 5 (prev bq 3), ask unchanged with aq 2 (prev 2)
        assert_eq!(ofi_step(100, 3, 110, 2, 101, 5, 110, 2), 5 - 2 + 2);
        assert_eq!(ofi_norm_1e9(4_000_000, 4_000_000, 4_000_000), SCALE_1E9);
        assert_eq!(ofi_norm_1e9(1, 0, 0), 1_000); // den floors at 1.0 qty
        assert_eq!(shift_px_1e6(100_000_000, SCALE_1E9, true), 100_010_000);
        assert_eq!(shift_px_1e6(100_000_000, SCALE_1E9, false), 99_990_000);
        assert_eq!(qty_for_notional_1e6(10_000_000_000, 100_000_000), 100_000_000);
        // composite: b + w·z with z = (f − mu)·inv_sd
        let p = IcdpSymParams {
            sym: BN,
            mu: [SCALE_1E9, 0, 0, 0, 0],
            inv_sd: [2 * SCALE_1E9, SCALE_1E9, SCALE_1E9, SCALE_1E9, SCALE_1E9],
            w: [SCALE_1E9 / 2, 0, 0, 0, 0],
            b: 100,
            thr: 1,
            notional_1e6: 1,
            spread_cap_1e9: 1,
            entry_slip_1e9: 0,
            exit_slip_1e9: 0,
        };
        // f0 = 3 ⇒ z = (3−1)·2 = 4 ⇒ s = 100 + 0.5·4 = 102 (×1e9 units)
        assert_eq!(
            composite_1e9(&p, &[3 * SCALE_1E9, 0, 0, 0, 0]),
            100 + 2 * SCALE_1E9
        );
    }

    // ---------- the bar machine ----------

    #[test]
    fn entry_ioc_fires_at_the_decision_and_exit_ioc_at_the_roll() {
        let mut s = strat(&[BN], SCALE_1E9); // thr 1 bps on r_early
        let mut c = ctx();
        let o = t0(&s);
        // Warm bar (prev): quotes inside bar 0.
        s.on_tick(&tick(BN, o - TF + MS, 100_000_000, 1_000_000, 100_010_000, 1_000_000, 0), &mut c);
        s.on_tick(&tick(BN, o - TF + 2 * MS, 100_000_000, 1_000_000, 100_010_000, 1_000_000, 0), &mut c);
        assert!(c.orders.is_empty());
        // Bar 1 opens on the prevailing quote (mid 100.005). A +2 bps
        // move before δ, then the first tick at/after δ decides.
        s.on_tick(&tick(BN, o + MS, 100_020_000, 2_000_000, 100_030_000, 1_000_000, 0), &mut c);
        assert!(c.orders.is_empty(), "before δ nothing fires");
        s.on_tick(&tick(BN, o + DELTA, 100_020_000, 2_000_000, 100_030_000, 1_000_000, 0), &mut c);
        assert_eq!(c.orders.len(), 1, "entry at the decision tick");
        let e = c.orders[0];
        assert_eq!(e.kind, ORDER_KIND_IOC);
        assert_eq!(e.side as u8, Side::Bid as u8);
        assert_eq!(e.px.raw(), shift_px_1e6(100_030_000, SCALE_1E9, true));
        assert_eq!(e.qty.raw(), qty_for_notional_1e6(10_000_000_000, e.px.raw()));
        assert_eq!(e.ttl_ns, TF - DELTA, "the intent dies with its bar");
        assert_eq!(e.ts_ns, o + DELTA);
        assert_eq!(s.position_side(BN), POS_LONG);
        assert_eq!(s.open_notional_total_1e6(), 10_000_000_000);
        // Later ticks in the same bar: no second entry.
        s.on_tick(&tick(BN, o + DELTA + MS, 100_050_000, 1_000_000, 100_060_000, 1_000_000, 0), &mut c);
        assert_eq!(c.orders.len(), 1);
        // The first tick of bar 2 rolls: exit IoC (ASK at bid − 2 bps), qty = position.
        s.on_tick(&tick(BN, o + TF + 5 * MS, 100_050_000, 1_000_000, 100_060_000, 1_000_000, 0), &mut c);
        assert_eq!(c.orders.len(), 2);
        let x = c.orders[1];
        assert_eq!(x.side as u8, Side::Ask as u8);
        assert_eq!(x.kind, ORDER_KIND_IOC);
        assert_eq!(x.px.raw(), shift_px_1e6(100_050_000, 2 * SCALE_1E9, false));
        assert_eq!(x.qty.raw(), e.qty.raw());
        assert_eq!(x.ttl_ns, TF - 5 * MS);
        assert_eq!(s.position_side(BN), POS_NONE);
        assert_eq!(s.open_notional_total_1e6(), 0);
        let k = s.counters();
        assert_eq!((k.decisions, k.signals, k.intents, k.exits), (1, 1, 1, 1));
        assert_eq!(s.orders_emitted(), 2);
    }

    #[test]
    fn short_entry_on_a_negative_composite_and_no_fire_under_threshold() {
        let mut s = strat(&[BN], 3 * SCALE_1E9); // thr 3 bps
        let mut c = ctx();
        let o = t0(&s);
        s.on_tick(&tick(BN, o - TF + MS, 100_000_000, 1_000_000, 100_010_000, 1_000_000, 0), &mut c);
        // −2 bps: under the threshold ⇒ no fire.
        s.on_tick(&tick(BN, o + DELTA, 99_980_000, 1_000_000, 99_990_000, 1_000_000, 0), &mut c);
        assert!(c.orders.is_empty());
        assert_eq!(s.counters().decisions, 1);
        assert_eq!(s.counters().signals, 0);
        // Next bar: −5 bps vs its open ⇒ short.
        s.on_tick(&tick(BN, o + TF + MS, 99_980_000, 1_000_000, 99_990_000, 1_000_000, 0), &mut c);
        s.on_tick(&tick(BN, o + TF + DELTA, 99_930_000, 1_000_000, 99_940_000, 1_000_000, 0), &mut c);
        assert_eq!(c.orders.len(), 1);
        assert_eq!(c.orders[0].side as u8, Side::Ask as u8);
        assert_eq!(c.orders[0].px.raw(), shift_px_1e6(99_930_000, SCALE_1E9, false));
        assert_eq!(s.position_side(BN), POS_SHORT);
    }

    #[test]
    fn stale_ticks_hold_the_bar_and_the_exit_still_leaves_at_the_roll() {
        let mut s = strat(&[BN], SCALE_1E9);
        let mut c = ctx();
        let o = t0(&s);
        s.on_tick(&tick(BN, o - TF + MS, 100_000_000, 1_000_000, 100_010_000, 1_000_000, 0), &mut c);
        // Bar 1: a stale tick inside the bar before δ ⇒ decision skipped.
        s.on_tick(&tick(BN, o + MS, 100_020_000, 1_000_000, 100_030_000, 1_000_000, TICK_FLAG_STALE), &mut c);
        s.on_tick(&tick(BN, o + DELTA, 100_020_000, 1_000_000, 100_030_000, 1_000_000, 0), &mut c);
        assert!(c.orders.is_empty());
        assert_eq!(s.counters().skipped_stale_dec, 1);
        // Bar 2 opens on a STALE prevailing quote ⇒ skipped_stale_open.
        s.on_tick(&tick(BN, o + TF - MS, 100_020_000, 1_000_000, 100_030_000, 1_000_000, TICK_FLAG_STALE), &mut c);
        s.on_tick(&tick(BN, o + TF + DELTA, 100_060_000, 1_000_000, 100_070_000, 1_000_000, 0), &mut c);
        assert!(c.orders.is_empty());
        assert_eq!(s.counters().skipped_stale_open, 1);
        // Bar 3: prev bar (2) had a stale open ⇒ prev invalid ⇒ skipped_prev.
        s.on_tick(&tick(BN, o + 2 * TF + DELTA, 100_100_000, 1_000_000, 100_110_000, 1_000_000, 0), &mut c);
        assert_eq!(s.counters().skipped_prev, 1);
        // Bar 4: clean ⇒ enter long; then the roll happens on a STALE tick:
        // the exit is still emitted (fill deferred by the harness).
        s.on_tick(&tick(BN, o + 3 * TF + DELTA, 100_150_000, 1_000_000, 100_160_000, 1_000_000, 0), &mut c);
        assert_eq!(c.orders.len(), 1);
        s.on_tick(&tick(BN, o + 4 * TF + MS, 100_150_000, 1_000_000, 100_160_000, 1_000_000, TICK_FLAG_STALE), &mut c);
        assert_eq!(c.orders.len(), 2, "exit emitted at the roll regardless");
        assert_eq!(s.counters().exits, 1);
        assert_eq!(s.counters().exit_on_stale, 0, "the roll tick itself was stale, the last quote before it fresh");
        assert_eq!(s.position_side(BN), POS_NONE);
    }

    #[test]
    fn late_decision_spread_cap_and_table_cap_skip_with_counters() {
        let mut s = strat(&[BN, OKX], SCALE_1E9);
        let mut c = ctx();
        let o = t0(&s);
        for sym in [BN, OKX] {
            s.on_tick(&tick(sym, o - TF + MS, 100_000_000, 1_000_000, 100_010_000, 1_000_000, 0), &mut c);
        }
        // BN: first tick after δ lands in the last fifth ⇒ late.
        s.on_tick(&tick(BN, o + TF / 5 * 4, 100_050_000, 1_000_000, 100_060_000, 1_000_000, 0), &mut c);
        assert_eq!(s.counters().late_bars, 1);
        // OKX: spread 10 bps > cap 5 ⇒ skipped_spread.
        s.on_tick(&tick(OKX, o + DELTA, 100_000_000, 1_000_000, 100_100_000, 1_000_000, 0), &mut c);
        assert_eq!(s.counters().skipped_spread, 1);
        assert!(c.orders.is_empty());
        // Table cap: shrink the cap by pre-loading open notional.
        s.open_notional_total_1e6 = CAP_TABLE_1E6 - 1;
        // Bar 2 opens on the last bar-1 quote (mid 100.055); +9.5 bps at δ.
        s.on_tick(&tick(BN, o + TF + MS, 100_000_000, 1_000_000, 100_010_000, 1_000_000, 0), &mut c);
        s.on_tick(&tick(BN, o + TF + DELTA, 100_150_000, 1_000_000, 100_160_000, 1_000_000, 0), &mut c);
        assert_eq!(s.counters().signals, 1);
        assert_eq!(s.counters().caps_rejected, 1);
        assert!(c.orders.is_empty());
    }

    #[test]
    fn foreign_syms_are_ignored_and_the_sweep_rolls_a_quiet_instrument() {
        let mut s = strat(&[BN], SCALE_1E9);
        let mut c = ctx();
        let o = t0(&s);
        let foreign = make_symbol_id(VenueId::Deribit, 9);
        s.on_tick(&tick(BN, o - TF + MS, 100_000_000, 1_000_000, 100_010_000, 1_000_000, 0), &mut c);
        s.on_tick(&tick(BN, o + DELTA, 100_050_000, 1_000_000, 100_060_000, 1_000_000, 0), &mut c);
        assert_eq!(c.orders.len(), 1, "long entered");
        // BN goes quiet; 256 foreign ticks after the close trigger the sweep.
        let mut t = o + TF + MS;
        for _ in 0..SWEEP_EVERY {
            s.on_tick(&tick(foreign, t, 1_000_000, 1, 1_010_000, 1, 0), &mut c);
            t += MS;
        }
        assert_eq!(c.orders.len(), 2, "the sweep emitted BN's exit without a BN tick");
        assert_eq!(c.orders[1].side as u8, Side::Ask as u8);
        assert_eq!(s.position_side(BN), POS_NONE);
    }

    #[test]
    fn ring_full_counts_a_drop_and_keeps_the_book_flat() {
        let mut s = strat(&[BN], SCALE_1E9);
        let mut c = ctx();
        c.full = true;
        let o = t0(&s);
        s.on_tick(&tick(BN, o - TF + MS, 100_000_000, 1_000_000, 100_010_000, 1_000_000, 0), &mut c);
        s.on_tick(&tick(BN, o + DELTA, 100_050_000, 1_000_000, 100_060_000, 1_000_000, 0), &mut c);
        assert_eq!(s.orders_dropped(), 1);
        assert_eq!(s.orders_emitted(), 0);
        assert_eq!(s.position_side(BN), POS_NONE, "no position without an accepted intent");
        assert_eq!(s.open_notional_total_1e6(), 0);
    }

    #[test]
    fn ofi_accumulates_only_over_fresh_consecutive_quotes_of_the_bar() {
        let mut s = strat(&[BN], 1_000 * SCALE_1E9); // never fires
        let mut c = ctx();
        let o = t0(&s);
        s.on_tick(&tick(BN, o - TF + MS, 100_000_000, 3_000_000, 100_010_000, 2_000_000, 0), &mut c);
        // bar 1 opens on that quote (ofi 0). Bid up with bq 5, ask same aq 2:
        s.on_tick(&tick(BN, o + MS, 100_001_000, 5_000_000, 100_010_000, 2_000_000, 0), &mut c);
        let i = s.slot(BN).unwrap();
        assert_eq!(s.syms[i].ofi_acc, 5_000_000 - 2_000_000 + 2_000_000);
        // a stale tick contributes nothing and breaks the chain
        s.on_tick(&tick(BN, o + 2 * MS, 100_001_000, 9_000_000, 100_010_000, 2_000_000, TICK_FLAG_STALE), &mut c);
        assert_eq!(s.syms[i].ofi_acc, 5_000_000);
        s.on_tick(&tick(BN, o + 3 * MS, 100_001_000, 9_000_000, 100_010_000, 2_000_000, 0), &mut c);
        assert_eq!(s.syms[i].ofi_acc, 5_000_000, "no increment against a stale previous quote");
    }

    proptest::proptest! {
        /// Bar ids never go backwards and every emitted intent carries
        /// kind 1, a positive px/qty and a ttl no longer than the bar.
        #[test]
        fn intents_are_ioc_with_bar_bounded_ttl_for_arbitrary_streams(
            moves in proptest::collection::vec((-50i64..50, 1i64..5_000_000, 1i64..5_000_000, 1u64..3_000, 0u8..8), 1..300),
        ) {
            let mut s = strat(&[BN], SCALE_1E9 / 2);
            let mut c = ctx();
            let mut t = t0(&s) - TF + MS;
            let mut px = 100_000_000i64;
            let mut last_bar = 0u64;
            for (dpx, bq, aq, dt, stale) in moves {
                px += dpx * 1_000;
                t += dt * MS;
                let flags = if stale == 0 { TICK_FLAG_STALE } else { 0 };
                s.on_tick(&tick(BN, t, px, bq, px + 10_000, aq, flags), &mut c);
                let i = s.slot(BN).unwrap();
                proptest::prop_assert!(s.syms[i].bar_id >= last_bar);
                last_bar = s.syms[i].bar_id;
            }
            for o in &c.orders {
                proptest::prop_assert_eq!(o.kind, ORDER_KIND_IOC);
                proptest::prop_assert!(o.px.raw() > 0 && o.qty.raw() > 0);
                proptest::prop_assert!(o.ttl_ns <= TF);
            }
            let k = s.counters();
            proptest::prop_assert!(k.intents >= k.exits);
            proptest::prop_assert!(s.open_notional_total_1e6() >= 0);
        }
    }
}
