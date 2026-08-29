// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # features — the VM2 feature engine (vm2-plan §1.1, V2)
//!
//! Per-sym, engine-resident feature state feeding the v2 grammar
//! evaluator (V3): live top-of-book, rolling minute-window stats,
//! funding-APR windows, options mark/IV, depth-derived values and
//! clock features. Fed exclusively through the vm's `Strategy`
//! callbacks, so the backtest harness reproduces every value by
//! replaying the same records through the same code (§1.5 parity law).
//!
//! ## Allocation doctrine
//!
//! ONE boxed [`FeatureState`] (~12 MiB) is allocated ZEROED at
//! [`FeatureState::new_boxed`] — boot only, heap-direct (a stack
//! round-trip of a 12 MiB POD would overflow the thread stack).
//! After that: zero allocation on every path, no drop (all-POD), all
//! access through claimed fixed slots. Pool exhaustion fails CLOSED
//! (the affected feature stays absent and a counter increments) —
//! absent-data-holds is the grammar's own law, so exhaustion degrades
//! to "rows referencing this sym hold".
//!
//! ## Time law
//!
//! The engine clock is MONOTONIC (`core-time::now_ns`). Windows,
//! funding prints and clock features are WALL-time concepts, so the
//! state maintains a venue-derived wall offset: every event carrying
//! `venue_time_ms > 0` refreshes `off_ms = venue_time_ms − mono_ms`.
//! Until the first such event, wall-derived features are ABSENT —
//! honest, and replay-identical (the offset derives from record
//! contents, never from a syscall; doctrine: no syscalls in the data
//! path, and boot wall-clock would not survive replay).
//!
//! ## Per-venue funding print law (the settled-print derivation)
//!
//! The live funding channels stream CURRENT rates, while the carry
//! law (and the worker's REST-fed `funding` table) count SETTLED
//! prints. The state derives prints venue-faithfully:
//!
//! * **OKX / Bybit / Binance** (discrete 8 h prints, events carry
//!   `v1` = next-funding-time ms): when a fresh event's `v1` ADVANCES
//!   past the latched one, the closing period settled — record one
//!   print at the OLD next-funding time with the last rate latched
//!   before the advance.
//! * **Deribit** (continuous funding, hourly REST samples of
//!   `interest_8h`): sample one print per venue-time HOUR from the
//!   Funding event, preferring `v1` (= `funding_8h ×1e9`, emitted
//!   since VM2 V2) over `v0` (`current_funding`, the pre-V2 capture's
//!   only field) so replays of old captures still work. The ÷8
//!   cadence law applies at accumulation via
//!   [`core_types::funding_print_divisor`].
//! * **Hyperliquid** (hourly funding, no Funding channel): funding
//!   rides `AssetCtx` events (`v0` = rate ×1e9, NO venue timestamp) —
//!   sample one print per WALL hour once the wall offset exists.
//! * **FundingSeed** (D-1): the worker pushes SETTLED prints (rate
//!   ×1e9 + venue print ms) after boot and for venue-dark Binance;
//!   inserts dedup against existing prints within half the venue's
//!   print period (30 min where the period is 0) so waiter retries
//!   and live-derived prints never double-count.
//!
//! ## Rolling stats
//!
//! Rolling mean/EMA/min/max/std are computed over LAST-OF-MINUTE mid
//! samples in a per-(sym, window) ring bound at table commit
//! ([`FeatureState::bind_roll`], V3 wires it). Stats are recomputed
//! LAZILY at read when the cached minute is stale — O(window ≤ 4320)
//! at most once per minute per entry, deterministic in replay, zero
//! alloc. `std` is the population standard deviation; EMA uses
//! α = 2/(n+1) over the in-window samples oldest→newest. Missing
//! minutes (no tick) are skipped (they are neither zero nor carried
//! forward) — the sample count is exposed and a window with zero
//! samples is ABSENT.

use core_types::{
    funding_period_s, funding_print_divisor, ChannelEvent, ChannelId, DepthTopK, FeatId,
    OptSummary, SymbolId, Tick, VenueId, DEPTH_FLAG_STALE, DEPTH_K, FUNDING_WINDOW_24H_MIN,
    FUNDING_WINDOW_72H_MIN, OPT_SUMMARY_FLAG_MARK_PX, ROLL_WINDOW_MAX_MIN,
};

/// Per-sym latest-value slots (open-addressed, power of two).
pub const FEAT_SYM_SLOTS: usize = 1024;
/// Rolling-stat pool entries (distinct (sym, window) pairs a
/// committed table may bind — the v2 validator refuses tables needing
/// more).
pub const ROLL_POOL_ENTRIES: usize = 256;
/// Ring length of one rolling entry, minutes (= the 3-day window cap).
pub const ROLL_RING_MINUTES: usize = ROLL_WINDOW_MAX_MIN as usize;
/// Funding blocks (syms with observed funding; pool-claimed).
pub const FUNDING_BLOCKS: usize = 256;
/// Prints per funding block (72 h of hourly samples ≈ 72; seeds may
/// backfill more; generous margin, ring-evicted oldest-first).
pub const FUNDING_RING_PRINTS: usize = 640;
/// Max distinct rolling windows one sym can bind (validator-enforced
/// at commit; the per-sym index list is this long).
pub const MAX_ROLL_PER_SYM: usize = 8;

const MS_PER_MIN: i64 = 60_000;
const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_DAY: i64 = 86_400_000;

/// One rolling (sym, window) entry. `ring[minute % win]` holds the
/// LAST mid ×1e9 of that wall minute (0 = no sample — a real mid of
/// exactly 0 cannot occur on a two-sided book).
#[repr(C)]
pub struct RollEntry {
    ring: [i64; ROLL_RING_MINUTES],
    /// Wall minute index of the newest written sample (0 = never).
    newest_min: i64,
    /// Minute the cache below was computed for (0 = never).
    cache_min: i64,
    /// Cached stats, ×1e9 (mean/ema/min/max/std over in-window
    /// samples at `cache_min`).
    mean_1e9: i64,
    ema_1e9: i64,
    min_1e9: i64,
    max_1e9: i64,
    std_1e9: i64,
    /// In-window sample count at `cache_min`.
    n: u32,
    /// Bound sym.
    sym: SymbolId,
    /// Window minutes (`[1, 4320]`); 0 = entry unused.
    win_min: u16,
    _pad: [u8; 6],
}

/// One funding print (settled or seeded).
#[repr(C)]
#[derive(Copy, Clone)]
struct FundingPrint {
    ts_ms: i64,
    rate_1e9: i64,
}

/// One sym's funding window state.
#[repr(C)]
pub struct FundingBlock {
    prints: [FundingPrint; FUNDING_RING_PRINTS],
    /// Ring head (next write index).
    head: u32,
    /// Live prints (≤ FUNDING_RING_PRINTS).
    count: u32,
    /// Cache freshness (wall minute; 0 = never).
    cache_min: i64,
    /// Cached APRs (fraction ×1e9) + in-window print counts.
    apr24_1e9: i64,
    apr72_1e9: i64,
    n24: u32,
    n72: u32,
    sym: SymbolId,
    /// 1 = claimed.
    used: u8,
    _pad: [u8; 3],
}

/// Per-sym latest-value block.
#[repr(C)]
pub struct FeatSym {
    sym: SymbolId,
    /// 1 = claimed (zeroed pool ⇒ unused).
    used: u8,
    /// DepthTopK STALE mirror.
    depth_stale: u8,
    /// OptSummary flags mirror.
    opt_flags: u8,
    _pad0: u8,
    /// Live top-of-book, px ×1e9 (0 = never ticked / one-sided).
    bid_1e9: i64,
    ask_1e9: i64,
    mid_1e9: i64,
    last_tick_ns: u64,
    /// Depth-derived, ×1e9 (imbalance fraction, spread bps, near
    /// notional USD).
    imb_1e9: i64,
    spread_bps_1e9: i64,
    near_1e9: i64,
    depth_ns: u64,
    /// Options mark/IV (raw venue units ×1e9 / fraction ×1e9).
    mark_px_1e9: i64,
    mark_iv_1e9: i64,
    opt_ns: u64,
    /// Latched next-funding-time ms (advance-law venues; 0 = none).
    next_funding_ms: i64,
    /// Rate latched with it (the print value when the period closes).
    pend_rate_1e9: i64,
    /// Last sampled hour (sample-law venues; 0 = never).
    last_hour: i64,
    /// Funding-block index +1 (0 = none claimed).
    fblock1: u16,
    /// Bound roll-entry indices +1 (0 = none).
    roll1: [u16; MAX_ROLL_PER_SYM],
    _pad1: [u8; 6],
}

/// The whole feature state — ONE boxed instance per vm.
#[repr(C)]
pub struct FeatureState {
    syms: [FeatSym; FEAT_SYM_SLOTS],
    roll: [RollEntry; ROLL_POOL_ENTRIES],
    funding: [FundingBlock; FUNDING_BLOCKS],
    /// Venue-derived wall−mono offset ms (i64::MIN = none yet).
    /// Stored as `off_ms + 1 − 1` with 0 meaning "unset" would
    /// collide with a legal offset, so a separate flag:
    off_ms: i64,
    off_set: u8,
    _pad: [u8; 7],
    /// Claimed counts (diagnostics + exhaustion tells).
    pub syms_used: u32,
    /// Roll entries bound.
    pub roll_used: u32,
    /// Funding blocks claimed.
    pub funding_used: u32,
    /// Fail-closed exhaustion counters (mirrored by the vm's §9
    /// family in V3).
    pub sym_slots_exhausted: u64,
    /// Roll-pool exhaustion at bind time.
    pub roll_exhausted: u64,
    /// Funding-pool exhaustion at first print.
    pub funding_exhausted: u64,
    /// Seeds deduplicated (already-known prints).
    pub seeds_deduped: u64,
    /// Funding prints recorded (all venues + seeds).
    pub prints_recorded: u64,
}

impl FeatureState {
    /// Heap-direct zeroed construction — boot only (module docs).
    pub fn new_boxed() -> Box<FeatureState> {
        let layout = core::alloc::Layout::new::<FeatureState>();
        // SAFETY: FeatureState is a `#[repr(C)]` POD of integers and
        // fixed arrays; the all-zero bit pattern is its documented
        // valid empty value (used flags 0, +1-indices 0 = none,
        // off_set 0). alloc_zeroed returns memory valid for the
        // layout; Box::from_raw takes sole ownership. A null return
        // aborts at boot (fail-fast: nothing ran yet).
        unsafe {
            let p = std::alloc::alloc_zeroed(layout).cast::<FeatureState>();
            assert!(!p.is_null(), "FeatureState boot allocation failed");
            Box::from_raw(p)
        }
    }

    // -----------------------------------------------------------
    // Sym slots (open-addressed, linear probe)
    // -----------------------------------------------------------

    #[inline(always)]
    fn slot_hash(sym: SymbolId) -> usize {
        // Venue byte folded into low bits (the symbol_bucket_mix
        // idea), then masked to the power-of-two table.
        let m = core_types::symbol_bucket_mix(sym);
        (m as usize) & (FEAT_SYM_SLOTS - 1)
    }

    /// Find the claimed slot for `sym`, if any.
    #[inline]
    fn find(&self, sym: SymbolId) -> Option<usize> {
        let mut i = Self::slot_hash(sym);
        let mut probes = 0;
        while probes < FEAT_SYM_SLOTS {
            let s = &self.syms[i];
            if s.used == 0 {
                return None;
            }
            if s.sym == sym {
                return Some(i);
            }
            i = (i + 1) & (FEAT_SYM_SLOTS - 1);
            probes += 1;
        }
        None
    }

    /// Find or claim the slot for `sym`. `None` = table full
    /// (counted; the caller's feature update is dropped — fail
    /// closed).
    #[inline]
    fn find_or_claim(&mut self, sym: SymbolId) -> Option<usize> {
        let mut i = Self::slot_hash(sym);
        let mut probes = 0;
        while probes < FEAT_SYM_SLOTS {
            let s = &self.syms[i];
            if s.used == 0 {
                let s = &mut self.syms[i];
                s.used = 1;
                s.sym = sym;
                self.syms_used += 1;
                return Some(i);
            }
            if s.sym == sym {
                return Some(i);
            }
            i = (i + 1) & (FEAT_SYM_SLOTS - 1);
            probes += 1;
        }
        self.sym_slots_exhausted = self.sym_slots_exhausted.wrapping_add(1);
        None
    }

    // -----------------------------------------------------------
    // Wall clock
    // -----------------------------------------------------------

    #[inline(always)]
    fn learn_wall(&mut self, venue_time_ms: u64, mono_ns: u64) {
        if venue_time_ms == 0 {
            return;
        }
        self.off_ms = venue_time_ms as i64 - (mono_ns / 1_000_000) as i64;
        self.off_set = 1;
    }

    /// Wall ms for a monotonic ns, if the offset is known.
    #[inline(always)]
    pub fn wall_ms(&self, mono_ns: u64) -> Option<i64> {
        if self.off_set == 0 {
            return None;
        }
        Some((mono_ns / 1_000_000) as i64 + self.off_ms)
    }

    // -----------------------------------------------------------
    // Ingest: ticks
    // -----------------------------------------------------------

    /// One BBO tick: refresh latest values and the bound rolling
    /// rings' minute samples. `mono_ns` is the engine clock at
    /// dispatch (`ctx.now_ns()`).
    #[inline]
    pub fn on_tick(&mut self, t: &Tick, mono_ns: u64) {
        let bid = t.bid_px.raw();
        let ask = t.ask_px.raw();
        let idx = match self.find_or_claim(t.sym) {
            Some(i) => i,
            None => return,
        };
        let two_sided = bid > 0 && ask > 0;
        let mid_1e9 = if two_sided {
            ((bid + ask) / 2) * 1_000
        } else {
            0
        };
        let wall = self.wall_ms(mono_ns);
        let s = &mut self.syms[idx];
        s.bid_1e9 = if bid > 0 { bid * 1_000 } else { 0 };
        s.ask_1e9 = if ask > 0 { ask * 1_000 } else { 0 };
        s.mid_1e9 = mid_1e9;
        s.last_tick_ns = mono_ns;
        // Minute sampling into bound rings (wall-gated; absent
        // offset ⇒ no samples — module time law).
        if !two_sided {
            return;
        }
        let minute = match wall {
            Some(w) => w / MS_PER_MIN,
            None => return,
        };
        let mut k = 0;
        while k < MAX_ROLL_PER_SYM {
            let r1 = self.syms[idx].roll1[k];
            if r1 != 0 {
                let e = &mut self.roll[(r1 - 1) as usize];
                let w = e.win_min as i64;
                debug_assert!(w > 0);
                let slot = (minute % w.max(1)) as usize
                    % ROLL_RING_MINUTES;
                if minute > e.newest_min {
                    // Entering a fresh minute: zero the slots of every
                    // SKIPPED minute (newest, minute) — capped at `w`
                    // slots, which covers the whole ring when the gap
                    // exceeds the window (proptest-caught law: the
                    // old `min(gap, w) − 1` cap left one stale slot to
                    // resurface as a phantom sample on ring wrap).
                    let clear_n = (minute - e.newest_min - 1).min(w);
                    let mut g = 1;
                    while g <= clear_n {
                        let dead = ((e.newest_min + g) % w) as usize % ROLL_RING_MINUTES;
                        e.ring[dead] = 0;
                        g += 1;
                    }
                    e.newest_min = minute;
                }
                if minute == e.newest_min {
                    e.ring[slot] = mid_1e9;
                }
                // An older-minute tick (mono/wall skew) is dropped —
                // samples never rewrite history.
            }
            k += 1;
        }
    }

    // -----------------------------------------------------------
    // Ingest: venue events (funding law) + seeds
    // -----------------------------------------------------------

    /// One venue event. Funding/AssetCtx feed the print law; every
    /// event teaches the wall offset.
    #[inline]
    pub fn on_venue_event(&mut self, e: &ChannelEvent, mono_ns: u64) {
        self.learn_wall(e.venue_time_ms, mono_ns);
        let venue = match VenueId::from_u8(e.venue) {
            Some(v) => v,
            None => return,
        };
        let ch = match ChannelId::from_u8(e.channel) {
            Some(c) => c,
            None => return,
        };
        match (venue, ch) {
            (VenueId::Okx | VenueId::Bybit | VenueId::Binance, ChannelId::Funding) => {
                let idx = match self.find_or_claim(e.sym) {
                    Some(i) => i,
                    None => return,
                };
                let next_ms = e.v1;
                let prev_next = self.syms[idx].next_funding_ms;
                let prev_rate = self.syms[idx].pend_rate_1e9;
                if prev_next > 0 && next_ms > prev_next {
                    // The old period closed: its latched rate settled.
                    self.record_print(idx, prev_next, prev_rate);
                }
                let s = &mut self.syms[idx];
                s.pend_rate_1e9 = e.v0;
                if next_ms > 0 {
                    s.next_funding_ms = next_ms;
                }
            }
            (VenueId::Deribit, ChannelId::Funding) => {
                // Hourly sample of funding_8h (v1, VM2 V2 parser) or
                // current_funding (v0, pre-V2 captures).
                if e.venue_time_ms == 0 {
                    return;
                }
                let idx = match self.find_or_claim(e.sym) {
                    Some(i) => i,
                    None => return,
                };
                let hour = e.venue_time_ms as i64 / MS_PER_HOUR;
                if self.syms[idx].last_hour != hour {
                    self.syms[idx].last_hour = hour;
                    let rate = if e.v1 != 0 { e.v1 } else { e.v0 };
                    self.record_print(idx, hour * MS_PER_HOUR, rate);
                }
            }
            (VenueId::Hyperliquid, ChannelId::AssetCtx) => {
                // Funding rides the ctx (v0 = rate ×1e9); no venue
                // ts — wall-hour sample once the offset exists.
                let wall = match self.wall_ms(mono_ns) {
                    Some(w) => w,
                    None => return,
                };
                let idx = match self.find_or_claim(e.sym) {
                    Some(i) => i,
                    None => return,
                };
                let hour = wall / MS_PER_HOUR;
                if self.syms[idx].last_hour != hour {
                    self.syms[idx].last_hour = hour;
                    self.record_print(idx, hour * MS_PER_HOUR, e.v0);
                }
            }
            _ => {}
        }
    }

    /// One FundingSeed (D-1): a SETTLED print from the worker.
    /// Dedup law in the module docs.
    #[inline]
    pub fn funding_seed(&mut self, sym: SymbolId, ts_ms: i64, rate_1e9: i64) {
        let idx = match self.find_or_claim(sym) {
            Some(i) => i,
            None => return,
        };
        let venue = VenueId::from_u8(core_types::symbol_venue_byte(sym));
        let period_ms = match venue {
            Some(v) => (funding_period_s(v) as i64) * 1_000,
            None => 0,
        };
        let tol = if period_ms > 0 {
            period_ms / 2
        } else {
            30 * MS_PER_MIN
        };
        if let Some(b1) = self.fblock_of(idx) {
            let b = &self.funding[b1];
            let mut i = 0;
            while i < b.count as usize {
                let p = b.prints[i];
                let d = (p.ts_ms - ts_ms).abs();
                if d < tol {
                    self.seeds_deduped = self.seeds_deduped.wrapping_add(1);
                    return;
                }
                i += 1;
            }
        }
        self.record_print(idx, ts_ms, rate_1e9);
    }

    #[inline(always)]
    fn fblock_of(&self, sym_idx: usize) -> Option<usize> {
        let b1 = self.syms[sym_idx].fblock1;
        if b1 == 0 {
            None
        } else {
            Some((b1 - 1) as usize)
        }
    }

    /// Append one print to the sym's funding block (claiming one on
    /// first use), invalidating the APR cache.
    fn record_print(&mut self, sym_idx: usize, ts_ms: i64, rate_1e9: i64) {
        let bidx = match self.fblock_of(sym_idx) {
            Some(b) => b,
            None => {
                // Claim the first free block.
                let mut b = 0;
                loop {
                    if b >= FUNDING_BLOCKS {
                        self.funding_exhausted = self.funding_exhausted.wrapping_add(1);
                        return;
                    }
                    if self.funding[b].used == 0 {
                        break;
                    }
                    b += 1;
                }
                self.funding[b].used = 1;
                self.funding[b].sym = self.syms[sym_idx].sym;
                self.syms[sym_idx].fblock1 = (b + 1) as u16;
                self.funding_used += 1;
                b
            }
        };
        let blk = &mut self.funding[bidx];
        let h = blk.head as usize;
        blk.prints[h] = FundingPrint { ts_ms, rate_1e9 };
        blk.head = ((h + 1) % FUNDING_RING_PRINTS) as u32;
        if (blk.count as usize) < FUNDING_RING_PRINTS {
            blk.count += 1;
        }
        blk.cache_min = 0; // invalidate
        self.prints_recorded = self.prints_recorded.wrapping_add(1);
    }

    // -----------------------------------------------------------
    // Ingest: depth + options
    // -----------------------------------------------------------

    /// One DepthTopK snapshot → imbalance / spread / near-notional
    /// (×1e9). STALE marks the features absent until the first clean
    /// post-resync snapshot (WS10-B gap law).
    #[inline]
    pub fn on_depth(&mut self, d: &DepthTopK, mono_ns: u64) {
        let idx = match self.find_or_claim(d.sym) {
            Some(i) => i,
            None => return,
        };
        if d.flags & DEPTH_FLAG_STALE != 0 {
            self.syms[idx].depth_stale = 1;
            return;
        }
        // Notionals in i128 (px ×1e6 × qty ×1e6 = ×1e12 domain).
        let mut bid_not: i128 = 0;
        let mut ask_not: i128 = 0;
        let mut k = 0;
        while k < DEPTH_K {
            let b = d.bids[k];
            let a = d.asks[k];
            bid_not += b.px_1e6 as i128 * b.qty_1e6 as i128;
            ask_not += a.px_1e6 as i128 * a.qty_1e6 as i128;
            k += 1;
        }
        let total = bid_not + ask_not;
        let s = &mut self.syms[idx];
        s.depth_stale = 0;
        s.depth_ns = mono_ns;
        s.imb_1e9 = if total > 0 {
            (((bid_not - ask_not) * 1_000_000_000) / total) as i64
        } else {
            0
        };
        // Spread bps of mid from the touch levels (both must exist).
        let bb = d.bids[0].px_1e6;
        let ba = d.asks[0].px_1e6;
        if bb > 0 && ba > 0 && d.bids[0].qty_1e6 > 0 && d.asks[0].qty_1e6 > 0 {
            let mid = (bb + ba) / 2;
            if mid > 0 {
                s.spread_bps_1e9 =
                    (((ba - bb) as i128 * 10_000 * 1_000_000_000) / mid as i128) as i64;
            }
        } else {
            s.spread_bps_1e9 = 0;
        }
        // Near notional: USD ×1e9 = ×1e12 / 1e3.
        s.near_1e9 = (total / 1_000) as i64;
    }

    /// One OptSummary record → mark px / IV latest values.
    #[inline]
    pub fn on_opt_summary(&mut self, o: &OptSummary, mono_ns: u64) {
        let idx = match self.find_or_claim(o.sym) {
            Some(i) => i,
            None => return,
        };
        let s = &mut self.syms[idx];
        s.mark_px_1e9 = o.mark_px_1e9;
        s.mark_iv_1e9 = o.mark_iv_1e9;
        s.opt_flags = o.flags;
        s.opt_ns = mono_ns;
    }

    // -----------------------------------------------------------
    // Roll binding (table-commit time; V3 calls this per row leg)
    // -----------------------------------------------------------

    /// Bind a rolling entry for `(sym, win_min)`, claiming a pool
    /// slot on first bind. Returns false on pool/per-sym exhaustion
    /// (counted; the validator's commit-time refusal makes this
    /// unreachable through validated tables — hand-built tables fail
    /// closed to ABSENT features).
    pub fn bind_roll(&mut self, sym: SymbolId, win_min: u16) -> bool {
        if win_min == 0 || win_min > ROLL_WINDOW_MAX_MIN {
            return false;
        }
        let idx = match self.find_or_claim(sym) {
            Some(i) => i,
            None => return false,
        };
        // Already bound?
        let mut k = 0;
        while k < MAX_ROLL_PER_SYM {
            let r1 = self.syms[idx].roll1[k];
            if r1 != 0 && self.roll[(r1 - 1) as usize].win_min == win_min {
                return true;
            }
            k += 1;
        }
        // Free per-sym index slot?
        let mut free_k = MAX_ROLL_PER_SYM;
        let mut k = 0;
        while k < MAX_ROLL_PER_SYM {
            if self.syms[idx].roll1[k] == 0 {
                free_k = k;
                break;
            }
            k += 1;
        }
        if free_k == MAX_ROLL_PER_SYM {
            self.roll_exhausted = self.roll_exhausted.wrapping_add(1);
            return false;
        }
        // Free pool entry?
        let mut e = 0;
        loop {
            if e >= ROLL_POOL_ENTRIES {
                self.roll_exhausted = self.roll_exhausted.wrapping_add(1);
                return false;
            }
            if self.roll[e].win_min == 0 {
                break;
            }
            e += 1;
        }
        let entry = &mut self.roll[e];
        entry.sym = sym;
        entry.win_min = win_min;
        entry.newest_min = 0;
        entry.cache_min = 0;
        entry.n = 0;
        self.syms[idx].roll1[free_k] = (e + 1) as u16;
        self.roll_used += 1;
        true
    }

    /// Drop every roll binding (a table flip rebinds from the new
    /// table — V3). Sample history is intentionally discarded: a new
    /// table's windows warm up honestly.
    pub fn clear_roll_bindings(&mut self) {
        let mut e = 0;
        while e < ROLL_POOL_ENTRIES {
            self.roll[e].win_min = 0;
            self.roll[e].newest_min = 0;
            self.roll[e].cache_min = 0;
            self.roll[e].n = 0;
            e += 1;
        }
        let mut i = 0;
        while i < FEAT_SYM_SLOTS {
            let mut k = 0;
            while k < MAX_ROLL_PER_SYM {
                self.syms[i].roll1[k] = 0;
                k += 1;
            }
            i += 1;
        }
        self.roll_used = 0;
    }

    // -----------------------------------------------------------
    // Reads (the V3 evaluator's API) — Option = the ABSENT law
    // -----------------------------------------------------------

    /// Read one feature for `sym` at engine time `mono_ns`.
    /// `win_min` applies to Roll* features only (must match a bound
    /// entry). Returns the ×1e9 signal-domain value, or `None` when
    /// the feature is ABSENT (no data / stale / no wall offset /
    /// unbound window).
    pub fn read(
        &mut self,
        feat: FeatId,
        sym: SymbolId,
        win_min: u16,
        mono_ns: u64,
    ) -> Option<i64> {
        let idx = self.find(sym)?;
        match feat {
            FeatId::Mid => {
                let v = self.syms[idx].mid_1e9;
                if self.syms[idx].last_tick_ns > 0 && v > 0 {
                    Some(v)
                } else {
                    None
                }
            }
            FeatId::Bid => {
                let v = self.syms[idx].bid_1e9;
                if self.syms[idx].last_tick_ns > 0 && v > 0 {
                    Some(v)
                } else {
                    None
                }
            }
            FeatId::Ask => {
                let v = self.syms[idx].ask_1e9;
                if self.syms[idx].last_tick_ns > 0 && v > 0 {
                    Some(v)
                } else {
                    None
                }
            }
            FeatId::RollMean | FeatId::RollEma | FeatId::RollMin | FeatId::RollMax
            | FeatId::RollStd => {
                let minute = self.wall_ms(mono_ns)? / MS_PER_MIN;
                let e = self.roll_entry_of(idx, win_min)?;
                self.refresh_roll(e, minute);
                let entry = &self.roll[e];
                if entry.n == 0 {
                    return None;
                }
                Some(match feat {
                    FeatId::RollMean => entry.mean_1e9,
                    FeatId::RollEma => entry.ema_1e9,
                    FeatId::RollMin => entry.min_1e9,
                    FeatId::RollMax => entry.max_1e9,
                    _ => entry.std_1e9,
                })
            }
            FeatId::Apr24 | FeatId::Apr72 => {
                let wall = self.wall_ms(mono_ns)?;
                let b = self.fblock_of(idx)?;
                self.refresh_apr(b, wall);
                let blk = &self.funding[b];
                if feat == FeatId::Apr24 {
                    if blk.n24 == 0 {
                        None
                    } else {
                        Some(blk.apr24_1e9)
                    }
                } else if blk.n72 == 0 {
                    None
                } else {
                    Some(blk.apr72_1e9)
                }
            }
            FeatId::MarkPx => {
                let s = &self.syms[idx];
                if s.opt_ns > 0 && s.opt_flags & OPT_SUMMARY_FLAG_MARK_PX != 0 {
                    Some(s.mark_px_1e9)
                } else {
                    None
                }
            }
            FeatId::MarkIv => {
                let s = &self.syms[idx];
                if s.opt_ns > 0 {
                    Some(s.mark_iv_1e9)
                } else {
                    None
                }
            }
            FeatId::DepthImb | FeatId::DepthSpreadBps | FeatId::DepthNearNotional => {
                let s = &self.syms[idx];
                if s.depth_ns == 0 || s.depth_stale != 0 {
                    return None;
                }
                Some(match feat {
                    FeatId::DepthImb => s.imb_1e9,
                    FeatId::DepthSpreadBps => s.spread_bps_1e9,
                    _ => s.near_1e9,
                })
            }
            FeatId::ClockToFunding => {
                let wall = self.wall_ms(mono_ns)?;
                let venue = VenueId::from_u8(core_types::symbol_venue_byte(sym))?;
                let period = funding_period_s(venue);
                if period == 0 {
                    return None; // continuous funding / no funding
                }
                let next_ms = if venue == VenueId::Hyperliquid {
                    // No venue-supplied next time: the next wall hour.
                    (wall / MS_PER_HOUR + 1) * MS_PER_HOUR
                } else {
                    let n = self.syms[idx].next_funding_ms;
                    if n <= 0 {
                        return None;
                    }
                    n
                };
                let secs_1e9 = (next_ms - wall).max(0) * 1_000_000;
                Some(secs_1e9)
            }
            FeatId::ClockUtcSod => {
                let wall = self.wall_ms(mono_ns)?;
                Some((wall.rem_euclid(MS_PER_DAY)) * 1_000_000)
            }
        }
    }

    #[inline]
    fn roll_entry_of(&self, sym_idx: usize, win_min: u16) -> Option<usize> {
        if win_min == 0 {
            return None;
        }
        let mut k = 0;
        while k < MAX_ROLL_PER_SYM {
            let r1 = self.syms[sym_idx].roll1[k];
            if r1 != 0 {
                let e = (r1 - 1) as usize;
                if self.roll[e].win_min == win_min {
                    return Some(e);
                }
            }
            k += 1;
        }
        None
    }

    /// Lazy stats recompute (module docs): O(win) walk when the
    /// cached minute is stale. In-window = minutes
    /// `(minute − win, minute]` — the CURRENT minute's running sample
    /// participates, matching "stats of the trailing window now".
    fn refresh_roll(&mut self, e: usize, minute: i64) {
        let entry = &mut self.roll[e];
        if entry.cache_min == minute {
            return;
        }
        let w = entry.win_min as i64;
        let mut n: u32 = 0;
        let mut sum: i64 = 0;
        let mut sumsq: i128 = 0;
        let mut mn: i64 = i64::MAX;
        let mut mx: i64 = i64::MIN;
        let mut ema: i64 = 0;
        // α = 2/(n+1) with n = window minutes, ×1e9 fixed point.
        let alpha_1e9: i64 = 2_000_000_000 / (w + 1);
        let mut m = minute - w + 1;
        while m <= minute {
            if m > 0 && m > entry.newest_min - w {
                let slot = (m % w) as usize % ROLL_RING_MINUTES;
                // A slot only belongs to minute `m` if it was written
                // for it: minutes newer than `newest_min` never
                // happened; anything else at the same slot index was
                // cleared on advance.
                if m <= entry.newest_min {
                    let v = entry.ring[slot];
                    if v > 0 {
                        n += 1;
                        sum += v;
                        sumsq += (v as i128) * (v as i128);
                        if v < mn {
                            mn = v;
                        }
                        if v > mx {
                            mx = v;
                        }
                        ema = if n == 1 {
                            v
                        } else {
                            ema + ((alpha_1e9 as i128 * (v - ema) as i128) / 1_000_000_000)
                                as i64
                        };
                    }
                }
            }
            m += 1;
        }
        entry.cache_min = minute;
        entry.n = n;
        if n == 0 {
            entry.mean_1e9 = 0;
            entry.ema_1e9 = 0;
            entry.min_1e9 = 0;
            entry.max_1e9 = 0;
            entry.std_1e9 = 0;
            return;
        }
        let mean = sum / n as i64;
        entry.mean_1e9 = mean;
        entry.ema_1e9 = ema;
        entry.min_1e9 = mn;
        entry.max_1e9 = mx;
        // Population variance = E[x²] − mean² (i128 exact).
        let ex2 = sumsq / n as i128;
        let var = ex2 - (mean as i128 * mean as i128);
        entry.std_1e9 = isqrt_i128(var.max(0));
    }

    /// Lazy APR recompute per wall minute (cache invalidated on every
    /// print insert too). Mirrors
    /// `claude_worker.carry_signal.apr_from_prints`: window =
    /// `[wall − W, wall]`, APR = Σ(rate)/divisor / days × 365.
    fn refresh_apr(&mut self, b: usize, wall_ms: i64) {
        let minute = wall_ms / MS_PER_MIN;
        let sym = self.funding[b].sym;
        if self.funding[b].cache_min == minute {
            return;
        }
        let div = match VenueId::from_u8(core_types::symbol_venue_byte(sym)) {
            Some(v) => funding_print_divisor(v),
            None => 1,
        };
        let lo24 = wall_ms - (FUNDING_WINDOW_24H_MIN as i64) * MS_PER_MIN;
        let lo72 = wall_ms - (FUNDING_WINDOW_72H_MIN as i64) * MS_PER_MIN;
        let blk = &mut self.funding[b];
        let mut sum24: i128 = 0;
        let mut sum72: i128 = 0;
        let mut n24: u32 = 0;
        let mut n72: u32 = 0;
        let mut i = 0;
        while i < blk.count as usize {
            let p = blk.prints[i];
            if p.ts_ms <= wall_ms {
                if p.ts_ms >= lo24 {
                    sum24 += p.rate_1e9 as i128;
                    n24 += 1;
                }
                if p.ts_ms >= lo72 {
                    sum72 += p.rate_1e9 as i128;
                    n72 += 1;
                }
            }
            i += 1;
        }
        blk.cache_min = minute;
        blk.n24 = n24;
        blk.n72 = n72;
        // apr = (Σ/div) / days × 365 — 24 h: ×365; 72 h: ×365/3.
        blk.apr24_1e9 = ((sum24 * 365) / div as i128) as i64;
        blk.apr72_1e9 = ((sum72 * 365) / (div as i128 * 3)) as i64;
    }

    /// Diagnostic: the funding-print count currently held for `sym`.
    pub fn funding_prints(&self, sym: SymbolId) -> u32 {
        match self.find(sym).and_then(|i| self.fblock_of(i)) {
            Some(b) => self.funding[b].count,
            None => 0,
        }
    }
}

/// Integer square root of a non-negative i128, returned as i64
/// (saturating — inputs are ≤ (1e9·px)² so the root fits easily).
#[inline]
fn isqrt_i128(v: i128) -> i64 {
    if v <= 0 {
        return 0;
    }
    let mut x = v;
    let mut y = (x + 1) >> 1;
    while y < x {
        x = y;
        y = (x + v / x) >> 1;
    }
    if x > i64::MAX as i128 {
        i64::MAX
    } else {
        x as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, Price, Qty};

    const MONO0: u64 = 100_000_000_000_000_000;
    /// A wall anchor: 2026-08-29 00:00:00 UTC in ms.
    const WALL0: u64 = 1_787_961_600_000;

    fn tick(sym: SymbolId, bid_1e6: i64, ask_1e6: i64) -> Tick {
        Tick::new(
            MONO0,
            VenueId::Okx,
            sym,
            1,
            Price::from_raw(bid_1e6),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask_1e6),
            Qty::from_raw(1_000_000),
        )
    }

    /// Teach the wall offset: one event with venue_time_ms at MONO0.
    fn teach_wall(f: &mut FeatureState) {
        let ev = ChannelEvent::new(
            MONO0,
            VenueId::Okx,
            ChannelId::Funding,
            make_symbol_id(VenueId::Okx, 999),
            0,
            WALL0,
            0,
            0,
        );
        f.on_venue_event(&ev, MONO0);
    }

    #[inline]
    fn mono_at(wall_ms: u64) -> u64 {
        MONO0 + (wall_ms - WALL0) * 1_000_000
    }

    // ---------------- construction / slots ----------------

    #[test]
    fn new_boxed_is_zeroed_and_empty() {
        let f = FeatureState::new_boxed();
        assert_eq!(f.syms_used, 0);
        assert_eq!(f.roll_used, 0);
        assert_eq!(f.funding_used, 0);
        assert_eq!(f.off_set, 0);
    }

    #[test]
    fn tick_read_roundtrip_and_absent_before_data() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Okx, 1);
        assert_eq!(f.read(FeatId::Mid, sym, 0, MONO0), None, "never seen");
        f.on_tick(&tick(sym, 64_990_000_000, 65_010_000_000), MONO0);
        assert_eq!(
            f.read(FeatId::Mid, sym, 0, MONO0),
            Some(65_000_000_000 * 1_000)
        );
        assert_eq!(
            f.read(FeatId::Bid, sym, 0, MONO0),
            Some(64_990_000_000_000)
        );
        assert_eq!(
            f.read(FeatId::Ask, sym, 0, MONO0),
            Some(65_010_000_000_000)
        );
    }

    #[test]
    fn one_sided_book_is_absent() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Okx, 2);
        f.on_tick(&tick(sym, 64_990_000_000, 0), MONO0);
        assert_eq!(f.read(FeatId::Mid, sym, 0, MONO0), None);
        assert_eq!(f.read(FeatId::Ask, sym, 0, MONO0), None);
        assert_eq!(
            f.read(FeatId::Bid, sym, 0, MONO0),
            Some(64_990_000_000_000),
            "the populated side reads"
        );
    }

    // ---------------- wall law ----------------

    #[test]
    fn wall_features_absent_until_offset_learned() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Okx, 3);
        f.on_tick(&tick(sym, 1_000_000, 1_002_000), MONO0);
        assert_eq!(f.read(FeatId::ClockUtcSod, sym, 0, MONO0), None);
        teach_wall(&mut f);
        let sod = f.read(FeatId::ClockUtcSod, sym, 0, MONO0).unwrap();
        assert_eq!(sod, 0, "WALL0 is a UTC midnight");
        let later = mono_at(WALL0 + 3_600_000);
        assert_eq!(
            f.read(FeatId::ClockUtcSod, sym, 0, later).unwrap(),
            3_600 * 1_000_000_000
        );
    }

    // ---------------- rolling stats ----------------

    #[test]
    fn roll_stats_match_naive_reference() {
        let mut f = FeatureState::new_boxed();
        teach_wall(&mut f);
        let sym = make_symbol_id(VenueId::Okx, 4);
        assert!(f.bind_roll(sym, 10));
        // 10 minutes of mids 101..110 (×1e6 px domain).
        let mut mids_1e9: Vec<i64> = Vec::new();
        for m in 0..10i64 {
            let px = 100_000_000 + (m + 1) * 1_000_000; // 101.. ×1e6
            let at = mono_at(WALL0 + (m as u64) * 60_000 + 30_000);
            let mut t = tick(sym, px - 5_000, px + 5_000);
            t.ts_ns = at;
            f.on_tick(&t, at);
            mids_1e9.push(px * 1_000);
        }
        let now = mono_at(WALL0 + 9 * 60_000 + 45_000);
        let mean = f.read(FeatId::RollMean, sym, 10, now).unwrap();
        let mn = f.read(FeatId::RollMin, sym, 10, now).unwrap();
        let mx = f.read(FeatId::RollMax, sym, 10, now).unwrap();
        let sd = f.read(FeatId::RollStd, sym, 10, now).unwrap();
        // Naive reference over the same 10 samples.
        let n = mids_1e9.len() as i64;
        let rsum: i64 = mids_1e9.iter().sum();
        let rmean = rsum / n;
        let rex2: i128 =
            mids_1e9.iter().map(|v| *v as i128 * *v as i128).sum::<i128>() / n as i128;
        let rvar = rex2 - (rmean as i128 * rmean as i128);
        assert_eq!(mean, rmean);
        assert_eq!(mn, *mids_1e9.iter().min().unwrap());
        assert_eq!(mx, *mids_1e9.iter().max().unwrap());
        assert_eq!(sd, isqrt_i128(rvar.max(0)));
        // EMA: oldest→newest, α = 2/11.
        let alpha = 2_000_000_000i64 / 11;
        let mut rema = mids_1e9[0];
        let mut i = 1;
        while i < mids_1e9.len() {
            rema += ((alpha as i128 * (mids_1e9[i] - rema) as i128) / 1_000_000_000) as i64;
            i += 1;
        }
        assert_eq!(f.read(FeatId::RollEma, sym, 10, now).unwrap(), rema);
    }

    #[test]
    fn roll_window_slides_and_skips_missing_minutes() {
        let mut f = FeatureState::new_boxed();
        teach_wall(&mut f);
        let sym = make_symbol_id(VenueId::Okx, 5);
        assert!(f.bind_roll(sym, 3));
        // Samples in minutes 0 and 2 (minute 1 silent).
        for (m, px) in [(0u64, 100_000_000i64), (2, 104_000_000)] {
            let at = mono_at(WALL0 + m * 60_000 + 1_000);
            let mut t = tick(sym, px - 1_000, px + 1_000);
            t.ts_ns = at;
            f.on_tick(&t, at);
        }
        let at2 = mono_at(WALL0 + 2 * 60_000 + 30_000);
        // Window (minute 0..=2): two samples, mean of 100/104 ×1e9.
        assert_eq!(
            f.read(FeatId::RollMean, sym, 3, at2).unwrap(),
            102_000_000_000
        );
        // Advance to minute 3: minute-0 sample leaves the window.
        let at3 = mono_at(WALL0 + 3 * 60_000 + 5_000);
        assert_eq!(
            f.read(FeatId::RollMean, sym, 3, at3).unwrap(),
            104_000_000_000,
            "only the minute-2 sample remains"
        );
        // Advance far: everything expires ⇒ ABSENT.
        let at9 = mono_at(WALL0 + 9 * 60_000);
        assert_eq!(f.read(FeatId::RollMean, sym, 3, at9), None);
    }

    #[test]
    fn roll_unbound_window_is_absent_and_bind_validates() {
        let mut f = FeatureState::new_boxed();
        teach_wall(&mut f);
        let sym = make_symbol_id(VenueId::Okx, 6);
        f.on_tick(&tick(sym, 1_000_000, 1_002_000), mono_at(WALL0 + 500));
        assert_eq!(f.read(FeatId::RollMean, sym, 10, mono_at(WALL0 + 1_000)), None);
        assert!(!f.bind_roll(sym, 0), "window 0 refused");
        assert!(
            !f.bind_roll(sym, ROLL_WINDOW_MAX_MIN + 1),
            "over-cap window refused"
        );
        assert!(f.bind_roll(sym, 60));
        assert!(f.bind_roll(sym, 60), "re-bind of the same pair is idempotent");
        assert_eq!(f.roll_used, 1);
    }

    // ---------------- funding: advance law ----------------

    #[test]
    fn okx_advance_law_records_settled_print() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Okx, 7);
        let t_print = WALL0 + 8 * 3_600_000; // 08:00Z print
        // Period open: rate updates latch, no print.
        let e1 = ChannelEvent::new(
            MONO0,
            VenueId::Okx,
            ChannelId::Funding,
            sym,
            0,
            WALL0,
            100_000_000, // 0.1 predicted
            t_print as i64,
        );
        f.on_venue_event(&e1, MONO0);
        assert_eq!(f.funding_prints(sym), 0);
        let e2 = ChannelEvent::new(
            MONO0 + 1,
            VenueId::Okx,
            ChannelId::Funding,
            sym,
            0,
            WALL0 + 1_000,
            125_000_000, // final settled value
            t_print as i64,
        );
        f.on_venue_event(&e2, MONO0 + 1);
        assert_eq!(f.funding_prints(sym), 0, "same period: still latched");
        // Next-funding advances ⇒ the old period settled at the LAST
        // latched rate.
        let e3 = ChannelEvent::new(
            mono_at(t_print + 1_000),
            VenueId::Okx,
            ChannelId::Funding,
            sym,
            0,
            t_print + 1_000,
            90_000_000,
            (t_print + 8 * 3_600_000) as i64,
        );
        f.on_venue_event(&e3, mono_at(t_print + 1_000));
        assert_eq!(f.funding_prints(sym), 1);
        // APR over 24 h: one print of 0.125 ⇒ ×365.
        let now = mono_at(t_print + 2_000);
        assert_eq!(
            f.read(FeatId::Apr24, sym, 0, now).unwrap(),
            125_000_000i64 * 365
        );
    }

    #[test]
    fn deribit_hourly_sample_prefers_funding_8h() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Deribit, 8);
        let h0 = WALL0;
        // Two events in the same hour: ONE sample, from v1.
        for k in 0..2u64 {
            let ev = ChannelEvent::new(
                mono_at(h0 + k * 100),
                VenueId::Deribit,
                ChannelId::Funding,
                sym,
                0,
                h0 + k * 100,
                7_000_000,  // current_funding (ignored when v1 set)
                16_000_000, // funding_8h
            );
            f.on_venue_event(&ev, mono_at(h0 + k * 100));
        }
        assert_eq!(f.funding_prints(sym), 1);
        // Next hour ⇒ second sample.
        let ev = ChannelEvent::new(
            mono_at(h0 + 3_600_000),
            VenueId::Deribit,
            ChannelId::Funding,
            sym,
            0,
            h0 + 3_600_000,
            7_000_000,
            16_000_000,
        );
        f.on_venue_event(&ev, mono_at(h0 + 3_600_000));
        assert_eq!(f.funding_prints(sym), 2);
        // ÷8 law: Σ = 0.032 /8 = 0.004; ×365.
        let now = mono_at(h0 + 3_700_000);
        assert_eq!(
            f.read(FeatId::Apr24, sym, 0, now).unwrap(),
            (32_000_000i64 / 8) * 365
        );
    }

    #[test]
    fn deribit_pre_v2_capture_falls_back_to_v0() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Deribit, 9);
        let ev = ChannelEvent::new(
            MONO0,
            VenueId::Deribit,
            ChannelId::Funding,
            sym,
            0,
            WALL0,
            9_000_000, // current_funding only (old capture)
            0,
        );
        f.on_venue_event(&ev, MONO0);
        assert_eq!(f.funding_prints(sym), 1);
    }

    #[test]
    fn hl_asset_ctx_samples_per_wall_hour() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Hyperliquid, 10);
        // Without a wall offset: held.
        let ev = ChannelEvent::new(
            MONO0,
            VenueId::Hyperliquid,
            ChannelId::AssetCtx,
            sym,
            0,
            0, // HL ctx has no venue ts
            12_500,
            0,
        );
        f.on_venue_event(&ev, MONO0);
        assert_eq!(f.funding_prints(sym), 0, "no wall offset yet ⇒ hold");
        teach_wall(&mut f);
        f.on_venue_event(&ev, mono_at(WALL0 + 10));
        assert_eq!(f.funding_prints(sym), 1);
        // Same wall hour: no second sample.
        f.on_venue_event(&ev, mono_at(WALL0 + 20_000));
        assert_eq!(f.funding_prints(sym), 1);
    }

    // ---------------- funding: seeds + dedup ----------------

    #[test]
    fn funding_seed_inserts_and_dedups() {
        let mut f = FeatureState::new_boxed();
        teach_wall(&mut f);
        let sym = make_symbol_id(VenueId::Binance, 11);
        let t0 = (WALL0 - 8 * 3_600_000) as i64;
        f.funding_seed(sym, t0, 50_000_000);
        f.funding_seed(sym, t0 - 8 * MS_PER_HOUR, 60_000_000);
        assert_eq!(f.funding_prints(sym), 2);
        // Exact + near-duplicate (within half the 8 h period): both
        // deduped.
        f.funding_seed(sym, t0, 50_000_000);
        f.funding_seed(sym, t0 + 60_000, 51_000_000);
        assert_eq!(f.funding_prints(sym), 2);
        assert_eq!(f.seeds_deduped, 2);
        let now = mono_at(WALL0);
        // 24 h window holds both prints: (0.05+0.06) ×365.
        assert_eq!(
            f.read(FeatId::Apr24, sym, 0, now).unwrap(),
            110_000_000i64 * 365
        );
    }

    #[test]
    fn apr_windows_are_absent_without_prints_in_window() {
        let mut f = FeatureState::new_boxed();
        teach_wall(&mut f);
        let sym = make_symbol_id(VenueId::Binance, 12);
        // One print 30 h old: outside 24 h, inside 72 h.
        let t_old = WALL0 as i64 - 30 * MS_PER_HOUR;
        f.funding_seed(sym, t_old, 80_000_000);
        let now = mono_at(WALL0);
        assert_eq!(f.read(FeatId::Apr24, sym, 0, now), None, "empty 24 h window");
        // 72 h: Σ/3 days ×365 = 0.08/3×365.
        assert_eq!(
            f.read(FeatId::Apr72, sym, 0, now).unwrap(),
            80_000_000i64 * 365 / 3
        );
    }

    // ---------------- depth ----------------

    #[test]
    fn depth_features_compute_and_stale_gap_law_holds() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Okx, 13);
        let mut bids = [core_types::DepthLevel::EMPTY; DEPTH_K];
        let mut asks = [core_types::DepthLevel::EMPTY; DEPTH_K];
        // 3 units bid-side, 1 unit ask-side at px 100.
        bids[0] = core_types::DepthLevel {
            px_1e6: 100_000_000,
            qty_1e6: 3_000_000,
        };
        asks[0] = core_types::DepthLevel {
            px_1e6: 100_500_000,
            qty_1e6: 1_000_000,
        };
        let d = DepthTopK::new(MONO0, VenueId::Okx, sym, 0, bids, asks);
        f.on_depth(&d, MONO0);
        // imb = (300 − 100.5)/400.5 ≈ 0.4981…
        let imb = f.read(FeatId::DepthImb, sym, 0, MONO0).unwrap();
        let want = ((300_000_000_000_000i128 - 100_500_000_000_000)
            * 1_000_000_000
            / 400_500_000_000_000) as i64;
        assert_eq!(imb, want);
        // spread = 0.5 of mid 100.25 → ×1e4 bps.
        let sp = f.read(FeatId::DepthSpreadBps, sym, 0, MONO0).unwrap();
        let want_sp =
            ((500_000i128 * 10_000 * 1_000_000_000) / 100_250_000) as i64;
        assert_eq!(sp, want_sp);
        // near notional ×1e9 = total(×1e12)/1e3.
        assert_eq!(
            f.read(FeatId::DepthNearNotional, sym, 0, MONO0).unwrap(),
            400_500_000_000
        );
        // STALE ⇒ absent; clean snapshot restores.
        let d_stale = DepthTopK::new(MONO0 + 1, VenueId::Okx, sym, DEPTH_FLAG_STALE, bids, asks);
        f.on_depth(&d_stale, MONO0 + 1);
        assert_eq!(f.read(FeatId::DepthImb, sym, 0, MONO0 + 1), None);
        f.on_depth(&d, MONO0 + 2);
        assert_eq!(f.read(FeatId::DepthImb, sym, 0, MONO0 + 2), Some(want));
    }

    // ---------------- options ----------------

    #[test]
    fn opt_features_respect_flag_bits() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Okx, 14);
        // OKX opt-summary: NO mark px (flags 0) — IV present.
        let o = OptSummary::new(
            MONO0,
            VenueId::Okx,
            sym,
            0,
            0,
            654_300_000,
            65_000_000_000_000,
            0,
            500_000_000,
            1,
            1,
            -1,
        );
        f.on_opt_summary(&o, MONO0);
        assert_eq!(f.read(FeatId::MarkIv, sym, 0, MONO0), Some(654_300_000));
        assert_eq!(f.read(FeatId::MarkPx, sym, 0, MONO0), None, "flag absent");
        let sym2 = make_symbol_id(VenueId::Deribit, 15);
        let o2 = OptSummary::new(
            MONO0,
            VenueId::Deribit,
            sym2,
            OPT_SUMMARY_FLAG_MARK_PX,
            41_500_000,
            700_000_000,
            65_000_000_000_000,
            0,
            -400_000_000,
            2,
            3,
            -5,
        );
        f.on_opt_summary(&o2, MONO0);
        assert_eq!(f.read(FeatId::MarkPx, sym2, 0, MONO0), Some(41_500_000));
    }

    // ---------------- clock ----------------

    #[test]
    fn clock_to_funding_venue_laws() {
        let mut f = FeatureState::new_boxed();
        teach_wall(&mut f);
        // OKX: from the latched next-funding time.
        let sym = make_symbol_id(VenueId::Okx, 16);
        let t_print = (WALL0 + 4 * 3_600_000) as i64;
        let ev = ChannelEvent::new(
            MONO0,
            VenueId::Okx,
            ChannelId::Funding,
            sym,
            0,
            WALL0,
            100_000_000,
            t_print,
        );
        f.on_venue_event(&ev, MONO0);
        let now = mono_at(WALL0 + 3_600_000);
        assert_eq!(
            f.read(FeatId::ClockToFunding, sym, 0, now).unwrap(),
            3 * 3_600 * 1_000_000_000i64
        );
        // HL: next wall hour, no venue event needed beyond a slot.
        let hsym = make_symbol_id(VenueId::Hyperliquid, 17);
        f.on_tick(&tick(hsym, 1_000_000, 1_002_000), now);
        assert_eq!(
            f.read(FeatId::ClockToFunding, hsym, 0, mono_at(WALL0 + 3_540_000))
                .unwrap(),
            60 * 1_000_000_000i64,
            "one minute to the next wall hour"
        );
        // Deribit: continuous ⇒ absent.
        let dsym = make_symbol_id(VenueId::Deribit, 18);
        f.on_tick(&tick(dsym, 1_000_000, 1_002_000), now);
        assert_eq!(f.read(FeatId::ClockToFunding, dsym, 0, now), None);
    }

    // ---------------- exhaustion fail-closed ----------------

    #[test]
    fn roll_per_sym_exhaustion_fails_closed() {
        let mut f = FeatureState::new_boxed();
        let sym = make_symbol_id(VenueId::Okx, 19);
        let mut w = 1u16;
        while w <= MAX_ROLL_PER_SYM as u16 {
            assert!(f.bind_roll(sym, w * 10));
            w += 1;
        }
        assert!(!f.bind_roll(sym, 999), "9th window refused");
        assert_eq!(f.roll_exhausted, 1);
    }
}
