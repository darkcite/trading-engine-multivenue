// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `LongVolSet` — the engine's owner of the long-tenor HAR (HAR H3.3)
//!
//! H3 runs [`LongVolEngine`] in the ENGINE (ruling 2026-09-26): one engine
//! per `har.toml` series — at most [`LONG_SET_MAX`], the Hypercall
//! underlyings — fed its series' live mid once a minute. `StrategySet`
//! holds the set in the regime detector's seat and drives it from the same
//! two callbacks: every tick, and the set's 1 s timer.
//!
//! ## The minute
//!
//! A FRESH two-sided quote of a series' feed parks its mid — the floored
//! `(bid + ask) / 2`, the regime's and VRP's law — as the open minute's
//! latest, or as the next minute's first when it lands past the boundary
//! before the timer has rolled. The timer rolls every completed minute: a
//! series whose feed quoted in it delivers the minute's LAST mid, stamped
//! with the minute's open ([`LongVolEngine::on_minute_close_at`]); a minute
//! with no quote delivers nothing, and the next close forms the return
//! across it (a gap — the law VRP and BIN15 feed their engines by). A UTC
//! day with no quote at all is the engine's EMPTY day. The minute grid is
//! a 60 s clock anchored at configure, the regime's.
//!
//! ## The day close, staggered
//!
//! The first minute of a new UTC day closes the engine's open day — the
//! close law over the whole 1–40 d grid, ~38 µs a series on the M4 Pro
//! (bench `vol/long_day_close_warm`, warm rings). Twelve series would pay
//! ~0.45 ms in one poll at 00:01Z, the minute a BIN15 quarter-hour also
//! turns. So at most
//! [`DAY_CLOSES_PER_POLL`] series crosses a day per poll; the others HOLD
//! their first new-day minute — value and stamp — and release it at a
//! later poll, one a poll, before any later minute of theirs. Twelve
//! series are through by 00:01:12Z, long before the next roll. The values
//! are unchanged: a held minute is delivered with its own stamp, and a new
//! minute of a series still holding forces the held one out first (order
//! is the law; the budget is only the pacing). [`LongVolSet::drain_held`]
//! delivers whatever is still held at shutdown.
//!
//! ## The weekday profile
//!
//! Recomputed at every day close from the engine's own ring
//! ([`LongVolEngine::weekday_profile_1e6`]) and held beside it for
//! `/state` — published, never consumed.
//!
//! ## Doctrine
//!
//! * **Inert until configured:** one branch per tick and per poll, so a
//!   boot without `har.toml` is the pre-H3 engine, bit for bit.
//! * **No allocation after [`LongVolSet::configure`]** (bench gate 78):
//!   the engines are boxed there, once, at boot. Per tick: one hash probe
//!   (the regime's open-addressing map). Per minute: one close per
//!   quoting series. Per UTC day: the staggered closes.
//! * The crate forbids `unsafe`, so the per-series arrays are sized
//!   [`LONG_SET_CAP`] (a power of two) and every index is masked into
//!   them: no bounds check survives on the tick path.
//! * **Nothing here trades** (plan law L4): no member reads the set;
//!   `/state` and the state file do.

use core_time::{NsTs, WallAnchor};
use core_types::{SymbolId, Tick, SYMBOL_ID_NONE};

use crate::long::{LongVolEngine, DAY_MS, WEEKDAYS};

/// Series one set may run: the Hypercall underlyings (O-HC5) —
/// `core_config::har::HAR_MAX_SERIES`.
pub const LONG_SET_MAX: usize = 12;

/// The per-series arrays' length: the next power of two above
/// [`LONG_SET_MAX`], so a masked index is always in bounds.
pub const LONG_SET_CAP: usize = 16;

/// A series name is at most this many bytes (`core_config::har::HAR_NAME_MAX`).
pub const LONG_SET_NAME_MAX: usize = 12;

/// UTC day closes one poll may pay for (the stagger).
pub const DAY_CLOSES_PER_POLL: u32 = 1;

const MASK: usize = LONG_SET_CAP - 1;
/// Feed-map slots: a power of two, at least twice [`LONG_SET_MAX`] (a
/// miss probes ~1.8 slots at 12 of 32).
const MAP_SLOTS: usize = 32;
const MAP_BITS: u32 = MAP_SLOTS.trailing_zeros();
const SLOT_NONE: u8 = u8::MAX;
const MINUTE_NS: u64 = 60_000_000_000;
const MINUTE_MS: u64 = 60_000;

const _: () = assert!(LONG_SET_CAP.is_power_of_two() && LONG_SET_CAP >= LONG_SET_MAX);
const _: () = assert!(MAP_SLOTS.is_power_of_two() && MAP_SLOTS >= 2 * LONG_SET_MAX);
const _: () = assert!(LONG_SET_MAX < SLOT_NONE as usize);
const _: () = assert!(core::mem::align_of::<LongVolSet>() == 64);
const _: () = assert!(core::mem::size_of::<LongVolSet>() == 2_816);

/// One series to configure: its name and its feed's symbol.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LongSeries<'a> {
    /// 1..=12 bytes of `[A-Z0-9]` (`har.toml` `name`).
    pub name: &'a [u8],
    /// The feed's symbol, resolved against the boot universe.
    pub feed: SymbolId,
}

/// Why [`LongVolSet::configure`] refused (the set is left untouched).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LongSetErr {
    /// No series.
    Empty,
    /// More than [`LONG_SET_MAX`] series.
    TooMany(usize),
    /// Series `i`'s name is not 1..=12 bytes of `[A-Z0-9]`.
    BadName(usize),
    /// Series `i` has no feed symbol.
    NoFeed(usize),
    /// Series `i` repeats an earlier name.
    DuplicateName(usize),
    /// Series `i` repeats an earlier feed.
    DuplicateFeed(usize),
    /// The set is configured already (boot configures once).
    Configured,
}

impl core::fmt::Display for LongSetErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "no series"),
            Self::TooMany(n) => write!(f, "{n} series, at most {LONG_SET_MAX}"),
            Self::BadName(i) => write!(f, "series {i}: the name is not 1..=12 of [A-Z0-9]"),
            Self::NoFeed(i) => write!(f, "series {i}: no feed symbol"),
            Self::DuplicateName(i) => write!(f, "series {i}: the name repeats an earlier one"),
            Self::DuplicateFeed(i) => write!(f, "series {i}: the feed repeats an earlier one"),
            Self::Configured => write!(f, "configured already"),
        }
    }
}

impl std::error::Error for LongSetErr {}

/// What the set has done, for `/state` and the gauges.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LongSetCounters {
    /// Minute boundaries the clock rolled.
    pub minutes_rolled: u64,
    /// Minute closes delivered to the engines.
    pub closes: u64,
    /// Closes that crossed a UTC day (the close law ran).
    pub day_closes: u64,
    /// First new-day minutes held for a later poll (the stagger).
    pub held: u64,
    /// Held minutes forced out by a newer minute of their series.
    pub forced: u64,
    /// The costliest day close, ns (the engine's close law alone).
    pub day_close_ns_max: u64,
    /// The newest day close's cost, ns.
    pub day_close_ns_last: u64,
    /// Bumped at every day close and at a restore: the state writer's gate.
    pub epoch: u64,
}

/// The long-tenor HAR series of one engine (module doc).
#[repr(C, align(64))]
pub struct LongVolSet {
    // ---- the tick path: `n`, the clock's edge, the map, the minute slots
    /// Configured series; `0` = inert.
    n: u32,
    _pad0: u32,
    /// The open minute's end, monotonic ns.
    minute_end_mono: NsTs,
    /// Feed → series: open addressing, linear probe; empty = `SYMBOL_ID_NONE`.
    map_sym: [SymbolId; MAP_SLOTS],
    map_slot: [u8; MAP_SLOTS],
    /// The open minute's latest mid ×1e6 (valid while `cur_set`).
    cur_mid_1e6: [i64; LONG_SET_CAP],
    /// The first minute past the boundary: its latest mid and stamp.
    next_mid_1e6: [i64; LONG_SET_CAP],
    next_ts: [NsTs; LONG_SET_CAP],
    cur_set: [bool; LONG_SET_CAP],
    next_set: [bool; LONG_SET_CAP],
    // ---- per minute and per day
    /// The open minute's wall open, ms since the epoch.
    minute_ms: u64,
    anchor: WallAnchor,
    /// A held first-new-day minute: its mid and stamp (`held_ms == 0`: none).
    held_px_1e6: [i64; LONG_SET_CAP],
    held_ms: [u64; LONG_SET_CAP],
    feed: [SymbolId; LONG_SET_CAP],
    name: [[u8; LONG_SET_NAME_MAX]; LONG_SET_CAP],
    name_len: [u8; LONG_SET_CAP],
    profile_1e6: [[i64; WEEKDAYS]; LONG_SET_CAP],
    profile_n: [[u32; WEEKDAYS]; LONG_SET_CAP],
    /// Per series: bumped at each of its day closes and at the restore —
    /// the state writer rewrites `state-<NAME>.tsv` only when it moved.
    series_epoch: [u64; LONG_SET_CAP],
    counters: LongSetCounters,
    /// The engines, boxed at configure (~201 KiB each).
    engines: [Option<Box<LongVolEngine>>; LONG_SET_CAP],
}

impl Default for LongVolSet {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for LongVolSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LongVolSet")
            .field("n", &self.n)
            .field("minute_ms", &self.minute_ms)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

/// `(sym ^ sym >> 24) · φ`, top bits — the regime map's hash.
#[inline(always)]
const fn map_hash(sym: SymbolId) -> usize {
    ((sym ^ (sym >> 24)).wrapping_mul(0x9E37_79B1) >> (32 - MAP_BITS)) as usize & (MAP_SLOTS - 1)
}

/// 1..=12 bytes of `[A-Z0-9]`.
const fn valid_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > LONG_SET_NAME_MAX {
        return false;
    }
    let mut i = 0usize;
    while i < name.len() {
        if !(name[i].is_ascii_uppercase() || name[i].is_ascii_digit()) {
            return false;
        }
        i += 1;
    }
    true
}

impl LongVolSet {
    /// An inert set: no series, no engine, no clock.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            n: 0,
            _pad0: 0,
            minute_end_mono: 0,
            map_sym: [SYMBOL_ID_NONE; MAP_SLOTS],
            map_slot: [SLOT_NONE; MAP_SLOTS],
            cur_mid_1e6: [0; LONG_SET_CAP],
            next_mid_1e6: [0; LONG_SET_CAP],
            next_ts: [0; LONG_SET_CAP],
            cur_set: [false; LONG_SET_CAP],
            next_set: [false; LONG_SET_CAP],
            minute_ms: 0,
            anchor: WallAnchor {
                mono_ns: 0,
                wall_ns: 0,
            },
            held_px_1e6: [0; LONG_SET_CAP],
            held_ms: [0; LONG_SET_CAP],
            feed: [SYMBOL_ID_NONE; LONG_SET_CAP],
            name: [[0; LONG_SET_NAME_MAX]; LONG_SET_CAP],
            name_len: [0; LONG_SET_CAP],
            profile_1e6: [[0; WEEKDAYS]; LONG_SET_CAP],
            profile_n: [[0; WEEKDAYS]; LONG_SET_CAP],
            series_epoch: [0; LONG_SET_CAP],
            counters: LongSetCounters {
                minutes_rolled: 0,
                closes: 0,
                day_closes: 0,
                held: 0,
                forced: 0,
                day_close_ns_max: 0,
                day_close_ns_last: 0,
                epoch: 0,
            },
            engines: [const { None }; LONG_SET_CAP],
        }
    }

    /// Boot: validate the series, box one engine each, bind the feed map
    /// and anchor the minute clock at `now`. Refuses with the set
    /// untouched; configures once.
    pub fn configure(
        &mut self,
        series: &[LongSeries<'_>],
        anchor: WallAnchor,
        now: NsTs,
    ) -> Result<(), LongSetErr> {
        if self.n != 0 {
            return Err(LongSetErr::Configured);
        }
        if series.is_empty() {
            return Err(LongSetErr::Empty);
        }
        if series.len() > LONG_SET_MAX {
            return Err(LongSetErr::TooMany(series.len()));
        }
        let mut i = 0usize;
        while i < series.len() {
            let s = series[i];
            if !valid_name(s.name) {
                return Err(LongSetErr::BadName(i));
            }
            if s.feed == SYMBOL_ID_NONE {
                return Err(LongSetErr::NoFeed(i));
            }
            let mut j = 0usize;
            while j < i {
                if series[j].name == s.name {
                    return Err(LongSetErr::DuplicateName(i));
                }
                if series[j].feed == s.feed {
                    return Err(LongSetErr::DuplicateFeed(i));
                }
                j += 1;
            }
            i += 1;
        }
        let mut i = 0usize;
        while i < series.len() {
            let s = series[i];
            self.feed[i] = s.feed;
            self.name[i][..s.name.len()].copy_from_slice(s.name);
            self.name_len[i] = s.name.len() as u8;
            self.map_insert(s.feed, i as u8);
            self.engines[i] = Some(Box::new(LongVolEngine::new()));
            i += 1;
        }
        self.anchor = anchor;
        let minute = anchor.wall_of(now) / MINUTE_NS;
        self.minute_ms = minute * MINUTE_MS;
        self.minute_end_mono = anchor.mono_of((minute + 1) * MINUTE_NS);
        self.n = series.len() as u32;
        Ok(())
    }

    fn map_insert(&mut self, sym: SymbolId, slot: u8) {
        let mut h = map_hash(sym);
        while self.map_sym[h] != SYMBOL_ID_NONE {
            h = (h + 1) & (MAP_SLOTS - 1);
        }
        self.map_sym[h] = sym;
        self.map_slot[h] = slot;
    }

    /// The series `sym` feeds, or `SLOT_NONE`. The map is never full (at
    /// most 12 of 32), so an empty slot ends every miss.
    #[inline(always)]
    fn lookup(&self, sym: SymbolId) -> u8 {
        let mut h = map_hash(sym);
        let mut k = 0usize;
        while k < MAP_SLOTS {
            let s = self.map_sym[h];
            if s == sym {
                return self.map_slot[h];
            }
            if s == SYMBOL_ID_NONE {
                return SLOT_NONE;
            }
            h = (h + 1) & (MAP_SLOTS - 1);
            k += 1;
        }
        SLOT_NONE
    }

    /// Whether [`Self::configure`] has run.
    #[inline(always)]
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.n != 0
    }

    /// Configured series.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.n as usize
    }

    /// No series configured.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    // -----------------------------------------------------------------
    // The engine loop
    // -----------------------------------------------------------------

    /// One tick: a fresh two-sided quote of a series' feed parks its mid.
    /// Everything else costs one branch (inert) or one probe.
    #[inline(always)]
    pub fn on_tick(&mut self, t: &Tick) {
        if self.n == 0 {
            return;
        }
        let slot = self.lookup(t.sym);
        if slot == SLOT_NONE || t.is_stale() {
            return;
        }
        let bid = t.bid_px.raw();
        let ask = t.ask_px.raw();
        if bid <= 0 || ask <= 0 {
            return;
        }
        let k = slot as usize & MASK;
        let mid = (bid + ask) >> 1;
        if t.ts_ns < self.minute_end_mono {
            self.cur_mid_1e6[k] = mid;
            self.cur_set[k] = true;
        } else {
            self.next_mid_1e6[k] = mid;
            self.next_set[k] = true;
            self.next_ts[k] = t.ts_ns;
        }
    }

    /// The timer: release held minutes (the budget allowing), then roll
    /// every completed minute.
    #[inline]
    pub fn on_timer(&mut self, now: NsTs) {
        if self.n == 0 {
            return;
        }
        let mut budget = DAY_CLOSES_PER_POLL;
        self.release_held(&mut budget);
        while now >= self.minute_end_mono {
            self.roll(&mut budget);
        }
    }

    /// Held minutes, one per unit of budget, in series order.
    fn release_held(&mut self, budget: &mut u32) {
        let n = self.n as usize;
        let mut i = 0usize;
        while i < n && *budget > 0 {
            let k = i & MASK;
            if self.held_ms[k] != 0 {
                let (px, ms) = (self.held_px_1e6[k], self.held_ms[k]);
                self.held_ms[k] = 0;
                *budget -= 1;
                self.feed_engine(k, px, ms);
            }
            i += 1;
        }
    }

    /// Close the open minute for every series that quoted in it, then
    /// move the next minute's first quotes in.
    fn roll(&mut self, budget: &mut u32) {
        let minute_ms = self.minute_ms;
        let new_end = self.minute_end_mono + MINUTE_NS;
        let n = self.n as usize;
        let mut i = 0usize;
        while i < n {
            let k = i & MASK;
            if self.cur_set[k] {
                self.deliver(k, self.cur_mid_1e6[k], minute_ms, budget);
            }
            if self.next_set[k] && self.next_ts[k] < new_end {
                self.cur_mid_1e6[k] = self.next_mid_1e6[k];
                self.cur_set[k] = true;
                self.next_set[k] = false;
            } else {
                self.cur_set[k] = false;
            }
            i += 1;
        }
        self.minute_ms += MINUTE_MS;
        self.minute_end_mono = new_end;
        self.counters.minutes_rolled += 1;
    }

    /// One minute close for series `k`: a held minute goes first, and a
    /// day crossing past the poll's budget is held.
    fn deliver(&mut self, k: usize, px: i64, ms: u64, budget: &mut u32) {
        if self.held_ms[k] != 0 {
            let (held_px, held_ms) = (self.held_px_1e6[k], self.held_ms[k]);
            self.held_ms[k] = 0;
            self.counters.forced += 1;
            self.feed_engine(k, held_px, held_ms);
            self.feed_engine(k, px, ms);
            return;
        }
        if self.crosses(k, ms) {
            if *budget == 0 {
                self.held_px_1e6[k] = px;
                self.held_ms[k] = ms;
                self.counters.held += 1;
                return;
            }
            *budget -= 1;
        }
        self.feed_engine(k, px, ms);
    }

    /// Whether minute `ms` makes series `k`'s engine run its day-close
    /// law: it falls outside the open day, or no day is open over
    /// resident ones (a restore that ended on a closed day).
    fn crosses(&self, k: usize, ms: u64) -> bool {
        match &self.engines[k] {
            Some(e) => match e.open_day() {
                Some((day, _, _)) => ms - ms % DAY_MS != day,
                None => e.n_resident() > 0,
            },
            None => false,
        }
    }

    /// Hand one close to the engine; a crossing is timed, profiled and
    /// bumps the epoch.
    fn feed_engine(&mut self, k: usize, px: i64, ms: u64) {
        let crossing = self.crosses(k, ms);
        let Some(e) = self.engines[k].as_deref_mut() else {
            return;
        };
        self.counters.closes += 1;
        if !crossing {
            e.on_minute_close_at(px, ms);
            return;
        }
        let t0 = core_time::now_ns();
        e.on_minute_close_at(px, ms);
        let dt = core_time::now_ns().saturating_sub(t0);
        let (profile, n_days) = e.weekday_profile_1e6();
        self.profile_1e6[k] = profile;
        self.profile_n[k] = n_days;
        self.series_epoch[k] += 1;
        let c = &mut self.counters;
        c.day_closes += 1;
        c.epoch += 1;
        c.day_close_ns_last = dt;
        if dt > c.day_close_ns_max {
            c.day_close_ns_max = dt;
        }
    }

    /// Shutdown: deliver every held minute (its own stamp, its own
    /// value), so the final state write loses nothing.
    pub fn drain_held(&mut self) {
        let n = self.n as usize;
        let mut i = 0usize;
        while i < n {
            let k = i & MASK;
            if self.held_ms[k] != 0 {
                let (px, ms) = (self.held_px_1e6[k], self.held_ms[k]);
                self.held_ms[k] = 0;
                self.feed_engine(k, px, ms);
            }
            i += 1;
        }
    }

    // -----------------------------------------------------------------
    // Boot restore and the readers (cold)
    // -----------------------------------------------------------------

    /// Series `i`'s engine, for the boot restore — `seed_*` then
    /// [`Self::restored`]. `None` past the configured series.
    pub fn engine_mut(&mut self, i: usize) -> Option<&mut LongVolEngine> {
        if i >= self.n as usize {
            return None;
        }
        self.engines[i & MASK].as_deref_mut()
    }

    /// After the boot restore: refit every engine once, form every
    /// profile, bump the epoch (the restored state is state to write).
    pub fn restored(&mut self) {
        let n = self.n as usize;
        let mut i = 0usize;
        while i < n {
            let k = i & MASK;
            if let Some(e) = self.engines[k].as_deref_mut() {
                e.refresh();
                let (profile, n_days) = e.weekday_profile_1e6();
                self.profile_1e6[k] = profile;
                self.profile_n[k] = n_days;
                self.series_epoch[k] += 1;
            }
            i += 1;
        }
        self.counters.epoch += 1;
    }

    /// Series `i`'s state epoch (`0` past the configured series).
    #[must_use]
    pub fn series_epoch(&self, i: usize) -> u64 {
        if i >= self.n as usize {
            return 0;
        }
        self.series_epoch[i & MASK]
    }

    /// Series `i`'s engine.
    #[must_use]
    pub fn engine(&self, i: usize) -> Option<&LongVolEngine> {
        if i >= self.n as usize {
            return None;
        }
        self.engines[i & MASK].as_deref()
    }

    /// Series `i`'s name.
    #[must_use]
    pub fn name(&self, i: usize) -> Option<&[u8]> {
        if i >= self.n as usize {
            return None;
        }
        let k = i & MASK;
        Some(&self.name[k][..self.name_len[k] as usize])
    }

    /// Series `i`'s feed symbol.
    #[must_use]
    pub fn feed(&self, i: usize) -> Option<SymbolId> {
        if i >= self.n as usize {
            return None;
        }
        Some(self.feed[i & MASK])
    }

    /// Series `i`'s weekday profile as of its newest day close or restore:
    /// the ratios ×1e6 (Monday first) and the observed days behind each.
    #[must_use]
    pub fn profile(&self, i: usize) -> Option<(&[i64; WEEKDAYS], &[u32; WEEKDAYS])> {
        if i >= self.n as usize {
            return None;
        }
        let k = i & MASK;
        Some((&self.profile_1e6[k], &self.profile_n[k]))
    }

    /// Series `i`'s held minute `(px_1e6, min_ts_ms)`, if any.
    #[must_use]
    pub fn held(&self, i: usize) -> Option<(i64, u64)> {
        if i >= self.n as usize {
            return None;
        }
        let k = i & MASK;
        if self.held_ms[k] == 0 {
            None
        } else {
            Some((self.held_px_1e6[k], self.held_ms[k]))
        }
    }

    /// The open minute's wall open, ms (`0` before configure).
    #[inline]
    #[must_use]
    pub const fn minute_ms(&self) -> u64 {
        self.minute_ms
    }

    /// What the set has done.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> LongSetCounters {
        self.counters
    }
}

#[cfg(test)]
mod tests;
