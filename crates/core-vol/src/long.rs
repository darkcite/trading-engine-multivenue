// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `LongVolEngine` — the HAR over whole days (HAR H1)
//!
//! [`VolEngine`] forecasts 15 m, 4 h and 8 h from a 1 536-minute return
//! ring. Nothing longer fits in it: a weekly or monthly HAR needs a month
//! of history, and a minute ring that long is half a megabyte per
//! underlying. This is its sibling for whole days — every tenor from 1 to
//! [`LONG_TAU_DAYS_MAX`] days (the 1 d / 1 w / 1 M of the long-tenor plan
//! and the O-HC5 grid that spans a Hypercall chain) — fed by the same
//! minute-close law but keeping ONE number per UTC day: that day's `Σ r²`.
//!
//! ## The law, with every scaling pinned
//!
//! ```text
//! r_k     = ret_bps_1e9(prev_close, close)                  bps ×1e9, per minute
//! S_d     = Σ r_k² over the minutes stamped in UTC day d
//! rv_w    = isqrt( Σ S over the last w completed days )     w ∈ LONG_WINDOWS_DAYS = {1, 7, 30}
//! v       = (rv_1²/1440 + rv_7²/10080 + rv_30²/43200) / 3   variance per minute
//! har_τ   = isqrt( v × τ_min )                              τ = 1 ..= 40 days
//! x_τ     = ln(har_τ)                                       ×1e9, ARMED at each day's close
//! y_τ     = ln(isqrt( Σ S over the τ days after the arm ))  what that x forecast
//! (a, b)  = ols_fit_1e9 over the tenor's last 128 (x, y)     VolEngine's fit, one body
//! fit     = a + b·x/1e9        raw = x                      the two forecasts, ×1e9 log
//! σ_ann   = exp(ln σ̂) × sqrt(525 960 / τ_min) / 1e13          annualised fraction ×1e9
//! ```
//!
//! Every step is the one [`VolEngine`] takes, in days: the fold's
//! `isqrt`-then-square-then-divide, the `ln_1e9`/`exp_1e9` pair, the
//! annualising law and [`ols_fit_1e9`] itself. `x` and `y` are `ln` of
//! raw `bps ×1e9` integers, so their shared constant cancels into `a`.
//!
//! ## Pairs are made by the clock, and they overlap
//!
//! There is no campaign to settle. At every UTC day close the engine
//! ARMS every tenor with the `x` formed from the days completed so far
//! (and the fitted forecast made from it), and SETTLES every tenor's arm
//! from `τ` closes ago against the `τ` days that have since completed.
//! A weekly forecast made every day is seven concurrent holds; the arm
//! records live BESIDE the day that armed them in the day ring, so the
//! hold `τ` days back is always resident ([`DAY_RING`] > 40). The pair
//! is keyed by the first day of its target window.
//!
//! The QLIKE tell compares the RAW fold (`b = 1`, `a = 0`) with the FIT
//! as it stood when the hold was armed — out of sample, like the H0
//! rolling measurement. Long-tenor law L2: a monthly σ̂ is a level, not a
//! call; the raw number and this score are published beside the fit.
//!
//! ## Days are calendar days, and an unobserved day is a hole
//!
//! The day ring is CONTIGUOUS since its last clear: a UTC day with no
//! minute at all is pushed EMPTY (`n = 0`) through the same close law, so
//! "the last w days" are always w calendar days and a target window is
//! always τ of them. **The empty-day law:** no return is formed ACROSS an
//! empty day (the next close only primes — a return spanning a day would
//! carry that day's variance into the next), a target window holding an
//! empty day forms no pair, and a fold window holding one forms no arm
//! (the engine is not warm). ABSENT DATA HOLDS, over biased pairs that
//! would sit in a 128-day ring for months: an outage costs the forecast
//! until 30 observed days have passed, or until a restore fills the hole
//! from a source the outage did not touch (the H3 seed is cut from
//! `candles.db`, fetched apart from the engine's feed). A silence of
//! [`DAY_RING`] days or more CLEARS the ring (nothing resident is
//! contiguous with what follows) but keeps every pair, fit and QLIKE row
//! — history, not state. A day short of 1 440 returns (a hole inside the
//! day, whose return spans it) is kept and flagged by its `n`;
//! non-contiguous minutes count as `gaps`, as in [`VolEngine`].
//!
//! ## Doctrine
//!
//! * **No allocation after [`LongVolEngine::new`]** (bench gate 77); no
//!   floats; `#![forbid(unsafe_code)]` holds for the crate.
//! * **Per minute: one return, one square, one add** — the fields it
//!   touches share the struct's first cache line. **Per UTC day:** the
//!   close law over the grid — ≤ 40 settles (a τ-day sum, 820 adds in
//!   all, one `ln` and two QLIKE `exp2` each, and a refit of ≤ 128
//!   pairs) and 40 arms (one `isqrt` and one `ln` each); measured ~63 µs
//!   a series on the review host. **Per tick: nothing** — the minute
//!   boundary is the owner's, never derived from a tick.
//! * **ABSENT DATA HOLDS.** Fewer than [`LONG_WARM_DAYS`] observed days
//!   and nothing forecasts; fewer than [`crate::MIN_PAIRS`] pairs and the
//!   tenor has no fit.
//! * **Restore is exact, never replayed, never repaired.** The `seed_*`
//!   entry points put back what the writer accessors lend out, verbatim,
//!   and refuse what the writer could never have produced (a "none" or an
//!   out-of-range log-vol, an impossible day sum); [`Self::refresh`]
//!   refits once after a restore (the first ACCEPTED live minute does it
//!   if the owner forgot). Nothing here is a trading signal (plan law L4).

use crate::{fx, ols_fit_1e9, qlike_1e9, trailing_means_1e9, VolEngine, QLIKE_RING};

/// Milliseconds in a UTC day.
pub const DAY_MS: u64 = 86_400_000;

/// Nanoseconds in a UTC day — the unit [`long_tenor_of`] takes τ in.
pub const DAY_NS: u64 = DAY_MS * 1_000_000;

/// Minute returns in a complete UTC day (the fold's per-day
/// denominator).
pub const DAY_MINUTES: u32 = 1440;

/// Days in a week — the weekday profile's width.
pub const WEEKDAYS: usize = 7;

/// The UTC weekday of an instant, Monday = 0 … Sunday = 6 (1970-01-01, day
/// 0 of the epoch, was a Thursday — index 3).
#[inline]
#[must_use]
pub const fn weekday_of(ts_ms: u64) -> usize {
    ((ts_ms / DAY_MS + 3) % WEEKDAYS as u64) as usize
}

/// Completed UTC days retained. 64 > 40 + 1: the arm made
/// [`LONG_TAU_DAYS_MAX`] closes ago and the target it settles against
/// are both still resident when the newest day is written, and so is
/// every day of the 30-day fold.
pub const DAY_RING: usize = 64;

/// Pairs per tenor — one per day, so 128 days of memory: the rolling
/// fit plan law L1 measured unbiased at every tenor.
pub const PAIR_RING_LONG: usize = 128;

/// The fold's windows, in COMPLETED days.
pub const LONG_WINDOWS_DAYS: [u32; 3] = [1, 7, 30];

/// Contiguous completed days the fold needs before any tenor forecasts:
/// the longest window (a partial 30-day sum is a shorter window wearing
/// its name — the rule `VolEngine::har_1e9` applies to 1 440 minutes).
pub const LONG_WARM_DAYS: u64 = LONG_WINDOWS_DAYS[LONG_WINDOWS_DAYS.len() - 1] as u64;

/// The longest tenor, in days. The grid is every whole day
/// `1 ..= LONG_TAU_DAYS_MAX` (O-HC5); tenor index `t` is `τ − 1` days.
pub const LONG_TAU_DAYS_MAX: usize = 40;

/// Each tenor's annualiser `round(sqrt(525 960 / τ_min) × 1e9)`, index
/// `τ − 1` — the 525 960-minute year and the rounding of
/// [`crate::ANNUALISE_15M_1E9`], [`crate::ANNUALISE_4H_1E9`] and
/// [`crate::ANNUALISE_8H_1E9`], which the same law reproduces (pinned).
/// 1 d = 19 111 514 854, 1 w = 7 223 473 640, 1 M = 3 489 269 264.
pub const ANNUALISE_LONG_1E9: [i64; LONG_TAU_DAYS_MAX] = annualiser_table();

/// "Nothing here": an arm that was not made, a fit that did not exist
/// when it was. `fx::LOG2_UNDEFINED` is the same value, so an undefined
/// `ln` can never masquerade as a forecast.
const NONE: i64 = i64::MIN;

/// The largest `|ln|` a restored log-vol may carry, ×1e9 — 100 in log.
/// Every `x` and `y` the law forms is `ln` of a positive u64, so lies in
/// `[0, 44.4]`, and every fit within a few of that: the bound refuses
/// only garbage, and keeps every sum and product of the fit, the fold and
/// QLIKE far inside `i128`.
const LN_ABS_MAX_1E9: i64 = 100_000_000_000;

/// The largest `Σ r²` a restored day may carry: 1e32, four orders above a
/// day of 50 %-a-minute moves (3.6e28) and far below where the fold's
/// `v · τ_min` could leave `i128`.
const SUM_SQ_MAX: i128 = 100_000_000_000_000_000_000_000_000_000_000;

const _: () = assert!(DAY_RING > LONG_TAU_DAYS_MAX);
const _: () = assert!(DAY_RING as u64 > LONG_WARM_DAYS);
const _: () = assert!(PAIR_RING_LONG >= crate::MIN_PAIRS);
const _: () = assert!(NONE == fx::LOG2_UNDEFINED);
const _: () = assert!(core::mem::align_of::<LongVolEngine>() == 64);
const _: () = assert!(core::mem::size_of::<LongVolEngine>() == 206_080);

/// `round(sqrt(525 960 / τ_min) × 1e9)` in integers: `k = ⌊√Q⌋` with
/// `Q = 525 960e18 / τ_min` (`⌊√⌊Q⌋⌋ = ⌊√Q⌋`), then `k + 1` iff
/// `Q ≥ (k + ½)²`, i.e. `4 · 525 960e18 ≥ τ_min · (2k + 1)²` — exact.
const fn annualiser_1e9(tau_min: u64) -> i64 {
    const YEAR_E18: i128 = 525_960 * 1_000_000_000_000_000_000;
    let tau = tau_min as i128;
    let k = core_regime::math::isqrt_i128(YEAR_E18 / tau) as i128;
    if 4 * YEAR_E18 >= tau * (2 * k + 1) * (2 * k + 1) {
        (k + 1) as i64
    } else {
        k as i64
    }
}

const fn annualiser_table() -> [i64; LONG_TAU_DAYS_MAX] {
    let mut t = [0i64; LONG_TAU_DAYS_MAX];
    let mut i = 0usize;
    while i < LONG_TAU_DAYS_MAX {
        t[i] = annualiser_1e9((i as u64 + 1) * DAY_MINUTES as u64);
        i += 1;
    }
    t
}

/// A tenor on the long grid: whole days, its length in minutes and its
/// precomputed annualiser (the [`crate::Tenor`] shape, in days).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LongTenor {
    /// τ in days, `1 ..= LONG_TAU_DAYS_MAX`.
    pub tau_days: u32,
    /// τ in minutes.
    pub tau_min: i64,
    /// `sqrt(525 960 / τ_min) × 1e9`.
    pub annualise_1e9: i64,
}

/// The ONLY tenors this engine forecasts: every whole day from 1 to
/// [`LONG_TAU_DAYS_MAX`]. Anything else — a fraction of a day, zero, a
/// longer horizon, the [`crate::tenor_of`] tenors — is `None`.
#[inline]
#[must_use]
pub const fn long_tenor_of(tau_ns: u64) -> Option<LongTenor> {
    if tau_ns == 0 || tau_ns % DAY_NS != 0 {
        return None;
    }
    let d = tau_ns / DAY_NS;
    if d > LONG_TAU_DAYS_MAX as u64 {
        return None;
    }
    Some(LongTenor {
        tau_days: d as u32,
        tau_min: (d * DAY_MINUTES as u64) as i64,
        annualise_1e9: ANNUALISE_LONG_1E9[(d - 1) as usize],
    })
}

/// Which of a tenor's two forecasts to read (both are always published —
/// plan law L2).
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LongForecast {
    /// The HAR fold as it stands: `ln σ̂ = x` (slope 1, intercept 0).
    Raw = 0,
    /// The rolling fit applied to it: `ln σ̂ = a + b·x/1e9`.
    Fit = 1,
}

/// A tenor's QLIKE tell: the trailing-[`QLIKE_RING`] mean QLIKE of the
/// raw fold against that of the fit made at arming time. Lower is
/// better; `fit_beats_raw` is armed only on a FULL window.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LongQlike {
    /// Settled holds scored (saturates at [`QLIKE_RING`]).
    pub n: u32,
    /// Mean QLIKE of the raw fold ×1e9.
    pub raw_mean_1e9: i64,
    /// Mean QLIKE of the fit ×1e9.
    pub fit_mean_1e9: i64,
    /// `fit_mean < raw_mean` over a full window.
    pub fit_beats_raw: bool,
}

/// The long-tenor engine. One per underlying series; ~201 KiB of inline
/// rings, so the owner boxes it once at boot and never again.
///
/// The per-minute path writes only the open-day fields, which lead the
/// struct and share its first cache line.
#[repr(C, align(64))]
pub struct LongVolEngine {
    /// `Σ r²` of the open day.
    cur_sq: i128,
    /// The previous close ×1e6; `0` = none yet (the next close primes).
    prev_px_1e6: i64,
    /// The open day's 00:00Z, ms; `0` = no day open.
    cur_day_ts_ms: u64,
    /// The newest folded minute, ms since the epoch; `0` = none.
    last_min_ts_ms: u64,
    /// Non-contiguous minutes (a splice: a restart's hole, an outage).
    gaps: u64,
    /// Closes refused: a non-positive price, a minute at or before the
    /// newest folded one, a day at or before the newest closed one.
    refused: u64,
    /// Returns folded into the open day.
    cur_n: u32,
    /// A restore changed pairs no refit has seen yet.
    dirty: bool,
    /// Days closed since the ring's last clear; day `g` sits in slot
    /// `g % DAY_RING`, and `[n_days − DAY_RING, n_days)` is resident.
    n_days: u64,
    /// `Σ r²` of each day.
    day_sq: [i128; DAY_RING],
    /// Each day's 00:00Z, ms.
    day_ts_ms: [u64; DAY_RING],
    /// Returns each day contributed (< 1 440 = a short day).
    day_n: [u32; DAY_RING],
    /// The `x` each tenor armed at this day's close; `NONE` = not armed.
    day_x: [[i64; LONG_TAU_DAYS_MAX]; DAY_RING],
    /// The fitted `ln σ̂` made at the same close; `NONE` = no fit then.
    day_fit: [[i64; LONG_TAU_DAYS_MAX]; DAY_RING],
    /// Per tenor: the fitted line ×1e9, and whether it is usable.
    a_1e9: [i64; LONG_TAU_DAYS_MAX],
    b_1e9: [i64; LONG_TAU_DAYS_MAX],
    fitted: [bool; LONG_TAU_DAYS_MAX],
    /// Per tenor ring cursors and counts.
    pair_head: [usize; LONG_TAU_DAYS_MAX],
    n_pairs: [usize; LONG_TAU_DAYS_MAX],
    q_head: [usize; LONG_TAU_DAYS_MAX],
    q_n: [usize; LONG_TAU_DAYS_MAX],
    /// Per tenor pairs ×1e9, keyed by the target's first day (ms).
    pair_x_1e9: [[i64; PAIR_RING_LONG]; LONG_TAU_DAYS_MAX],
    pair_y_1e9: [[i64; PAIR_RING_LONG]; LONG_TAU_DAYS_MAX],
    pair_ts_ms: [[u64; PAIR_RING_LONG]; LONG_TAU_DAYS_MAX],
    /// Per tenor trailing QLIKE ×1e9: the raw fold, the fit.
    q_raw_1e9: [[i64; QLIKE_RING]; LONG_TAU_DAYS_MAX],
    q_fit_1e9: [[i64; QLIKE_RING]; LONG_TAU_DAYS_MAX],
}

impl Default for LongVolEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LongVolEngine {
    /// A zeroed engine: no day, no pair, no fit.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cur_sq: 0,
            prev_px_1e6: 0,
            cur_day_ts_ms: 0,
            last_min_ts_ms: 0,
            gaps: 0,
            refused: 0,
            cur_n: 0,
            dirty: false,
            n_days: 0,
            day_sq: [0; DAY_RING],
            day_ts_ms: [0; DAY_RING],
            day_n: [0; DAY_RING],
            day_x: [[NONE; LONG_TAU_DAYS_MAX]; DAY_RING],
            day_fit: [[NONE; LONG_TAU_DAYS_MAX]; DAY_RING],
            a_1e9: [0; LONG_TAU_DAYS_MAX],
            b_1e9: [0; LONG_TAU_DAYS_MAX],
            fitted: [false; LONG_TAU_DAYS_MAX],
            pair_head: [0; LONG_TAU_DAYS_MAX],
            n_pairs: [0; LONG_TAU_DAYS_MAX],
            q_head: [0; LONG_TAU_DAYS_MAX],
            q_n: [0; LONG_TAU_DAYS_MAX],
            pair_x_1e9: [[0; PAIR_RING_LONG]; LONG_TAU_DAYS_MAX],
            pair_y_1e9: [[0; PAIR_RING_LONG]; LONG_TAU_DAYS_MAX],
            pair_ts_ms: [[0; PAIR_RING_LONG]; LONG_TAU_DAYS_MAX],
            q_raw_1e9: [[0; QLIKE_RING]; LONG_TAU_DAYS_MAX],
            q_fit_1e9: [[0; QLIKE_RING]; LONG_TAU_DAYS_MAX],
        }
    }

    // -----------------------------------------------------------------
    // The feed
    // -----------------------------------------------------------------

    /// One 1-minute close ×1e6, stamped with the minute it belongs to (ms
    /// since the epoch — the bar's own minute, as for
    /// [`VolEngine::on_minute_close_at`]). The first close of a new UTC
    /// day CLOSES the open one (the day law, module doc) before its own
    /// return — which spans midnight — is folded into the new day.
    ///
    /// Refused and counted: a non-positive price, a minute at or before
    /// the newest folded one (never folded twice), a day at or before the
    /// newest closed one.
    pub fn on_minute_close_at(&mut self, px_1e6: i64, min_ts_ms: u64) {
        if px_1e6 <= 0 || min_ts_ms <= self.last_min_ts_ms {
            self.refused = self.refused.wrapping_add(1);
            return;
        }
        if self.dirty {
            self.refresh();
        }
        let day_ts = min_ts_ms - min_ts_ms % DAY_MS;
        if day_ts != self.cur_day_ts_ms && !self.roll_to(day_ts) {
            self.refused = self.refused.wrapping_add(1);
            return;
        }
        if self.last_min_ts_ms > 0 && min_ts_ms != self.last_min_ts_ms + 60_000 {
            self.gaps = self.gaps.wrapping_add(1);
        }
        if self.prev_px_1e6 > 0 {
            let r = core_regime::math::ret_bps_1e9(self.prev_px_1e6, px_1e6) as i128;
            self.cur_sq += r * r;
            self.cur_n += 1;
        }
        self.prev_px_1e6 = px_1e6;
        self.last_min_ts_ms = min_ts_ms;
    }

    /// Close the open day (if any) and open `day_ts`, pushing every
    /// calendar day between them EMPTY through the same close law — or
    /// clearing the ring when the silence is [`DAY_RING`] days or longer.
    /// `false`: `day_ts` is at or before the newest closed day.
    fn roll_to(&mut self, day_ts: u64) -> bool {
        if self.cur_day_ts_ms != 0 {
            // The open day holds `last_min_ts_ms`, which the caller's
            // minute is strictly after: the new day is later.
            debug_assert!(day_ts > self.cur_day_ts_ms);
            self.close_day(self.cur_day_ts_ms, self.cur_sq, self.cur_n);
        } else if self.n_days > 0 && day_ts <= self.last_day_ts() {
            return false;
        }
        if self.n_days > 0 {
            let mut next = self.last_day_ts() + DAY_MS;
            if next < day_ts {
                // A whole UTC day passed unobserved: a return across it
                // would carry that day's variance into this one, so the
                // next close only primes (the empty-day law, module doc).
                self.prev_px_1e6 = 0;
            }
            if (day_ts - next) / DAY_MS >= DAY_RING as u64 {
                self.n_days = 0;
            } else {
                while next < day_ts {
                    self.close_day(next, 0, 0);
                    next += DAY_MS;
                }
            }
        }
        self.cur_day_ts_ms = day_ts;
        self.cur_sq = 0;
        self.cur_n = 0;
        true
    }

    /// The close law: push day `(day_ts, sq, n)`, then for every tenor
    /// settle the arm made `τ` closes ago and arm this close.
    fn close_day(&mut self, day_ts: u64, sq: i128, n: u32) {
        let g = self.n_days;
        let s = slot(g);
        self.day_ts_ms[s] = day_ts;
        self.day_sq[s] = sq;
        self.day_n[s] = n;
        self.n_days = g + 1;
        let v = self.fold();
        let mut t = 0usize;
        while t < LONG_TAU_DAYS_MAX {
            let tau = t as u64 + 1;
            if g >= tau {
                self.settle(t, g - tau);
            }
            let (x, fit) = self.arm(v, t);
            self.day_x[s][t] = x;
            self.day_fit[s][t] = fit;
            t += 1;
        }
    }

    /// Settle tenor `t`'s arm made at the close of day `arm_g` against
    /// the `τ` days after it: form the pair, score both forecasts, refit.
    fn settle(&mut self, t: usize, arm_g: u64) {
        let a = slot(arm_g);
        let x = self.day_x[a][t];
        if x == NONE {
            return;
        }
        let first = arm_g + 1;
        let tau = t as u64 + 1;
        let mut acc: i128 = 0;
        let mut g = first;
        while g < first + tau {
            let s = slot(g);
            if self.day_n[s] == 0 {
                // ABSENT DATA HOLDS: an unobserved day is a hole in the
                // target, not a quiet day.
                return;
            }
            acc += self.day_sq[s];
            g += 1;
        }
        let rv = core_regime::math::isqrt_i128(acc);
        if rv <= 0 {
            return;
        }
        let y = fx::ln_1e9(rv as u64);
        if y == fx::LOG2_UNDEFINED {
            return;
        }
        self.push_pair(t, self.day_ts_ms[slot(first)], x, y);
        let fit = self.day_fit[a][t];
        if fit != NONE {
            self.push_qlike(t, qlike_1e9(y, x), qlike_1e9(y, fit));
        }
        self.refit(t);
    }

    /// Tenor `t`'s arm from fold `v`: `(x, fitted ln σ̂)`, `NONE` where
    /// absent.
    fn arm(&self, v: i128, t: usize) -> (i64, i64) {
        if v <= 0 {
            return (NONE, NONE);
        }
        let tau_min = (t as i128 + 1) * DAY_MINUTES as i128;
        let har = core_regime::math::isqrt_i128(v * tau_min);
        if har <= 0 {
            return (NONE, NONE);
        }
        let x = fx::ln_1e9(har as u64);
        if x == fx::LOG2_UNDEFINED {
            return (NONE, NONE);
        }
        let fit = if self.fitted[t] {
            self.a_1e9[t] + Self::slope_term(self.b_1e9[t], x)
        } else {
            NONE
        };
        (x, fit)
    }

    /// `floor(b·x / 1e9)` — floored like `VolEngine::ln_sigma_hat_1e9`
    /// and the Python mirror.
    #[inline]
    fn slope_term(b_1e9: i64, x_1e9: i64) -> i64 {
        core_regime::math::floor_div(b_1e9 as i128 * x_1e9 as i128, 1_000_000_000) as i64
    }

    /// The fold's variance per minute over the resident days; `0` while
    /// the engine is not warm ([`Self::is_warm`]).
    fn fold(&self) -> i128 {
        if !self.is_warm() {
            return 0;
        }
        let mut mean_sq: i128 = 0;
        let mut acc: i128 = 0;
        let mut back = 0u64;
        let mut w = 0usize;
        while w < LONG_WINDOWS_DAYS.len() {
            let win = LONG_WINDOWS_DAYS[w] as u64;
            while back < win {
                acc += self.day_sq[slot(self.n_days - 1 - back)];
                back += 1;
            }
            let rv = core_regime::math::isqrt_i128(acc) as i128;
            mean_sq += rv * rv / (win as i128 * DAY_MINUTES as i128);
            w += 1;
        }
        mean_sq / LONG_WINDOWS_DAYS.len() as i128
    }

    fn push_pair(&mut self, t: usize, ts_ms: u64, x: i64, y: i64) {
        let h = self.pair_head[t];
        self.pair_ts_ms[t][h] = ts_ms;
        self.pair_x_1e9[t][h] = x;
        self.pair_y_1e9[t][h] = y;
        self.pair_head[t] = (h + 1) % PAIR_RING_LONG;
        if self.n_pairs[t] < PAIR_RING_LONG {
            self.n_pairs[t] += 1;
        }
    }

    fn push_qlike(&mut self, t: usize, raw: i64, fit: i64) {
        let h = self.q_head[t];
        self.q_raw_1e9[t][h] = raw;
        self.q_fit_1e9[t][h] = fit;
        self.q_head[t] = (h + 1) % QLIKE_RING;
        if self.q_n[t] < QLIKE_RING {
            self.q_n[t] += 1;
        }
    }

    fn refit(&mut self, t: usize) {
        let n = self.n_pairs[t];
        match ols_fit_1e9(&self.pair_x_1e9[t][..n], &self.pair_y_1e9[t][..n]) {
            Some((a, b)) => {
                self.a_1e9[t] = a;
                self.b_1e9[t] = b;
                self.fitted[t] = true;
            }
            None => self.fitted[t] = false,
        }
    }

    /// The newest closed day's 00:00Z (`n_days > 0`).
    #[inline]
    fn last_day_ts(&self) -> u64 {
        debug_assert!(self.n_days > 0);
        self.day_ts_ms[slot(self.n_days - 1)]
    }

    /// Tenor index of `tau_ns` on the grid.
    #[inline]
    fn tix(tau_ns: u64) -> Option<usize> {
        long_tenor_of(tau_ns).map(|t| t.tau_days as usize - 1)
    }

    // -----------------------------------------------------------------
    // State
    // -----------------------------------------------------------------

    /// Whether the fold can forecast: the newest [`LONG_WARM_DAYS`]
    /// closed days are resident and every one was OBSERVED — an empty day
    /// in the window is a hole, not a quiet day. Every tenor warms
    /// together.
    #[must_use]
    pub const fn is_warm(&self) -> bool {
        if self.n_days < LONG_WARM_DAYS {
            return false;
        }
        let mut back = 0u64;
        while back < LONG_WARM_DAYS {
            if self.day_n[slot(self.n_days - 1 - back)] == 0 {
                return false;
            }
            back += 1;
        }
        true
    }

    /// Non-contiguous minutes folded (a splice — normal once per restart,
    /// a problem if it keeps climbing).
    #[inline]
    #[must_use]
    pub const fn gaps(&self) -> u64 {
        self.gaps
    }

    /// Closes refused (see [`Self::on_minute_close_at`]).
    #[inline]
    #[must_use]
    pub const fn refused(&self) -> u64 {
        self.refused
    }

    /// The newest folded minute, ms since the epoch; `0` = none.
    #[inline]
    #[must_use]
    pub const fn last_min_ts_ms(&self) -> u64 {
        self.last_min_ts_ms
    }

    /// The close the next return is formed against; `0` = none.
    #[inline]
    #[must_use]
    pub const fn prev_px_1e6(&self) -> i64 {
        self.prev_px_1e6
    }

    // -----------------------------------------------------------------
    // Forecasts (cold: once per minute at most, never per tick)
    // -----------------------------------------------------------------

    /// The regressor `x = ln(har_τ)` ×1e9 armed at the newest close —
    /// also the RAW forecast of `ln` realised vol over the τ days from
    /// that close. `None` off the grid, cold, or before any close.
    #[must_use]
    pub fn x_1e9(&self, tau_ns: u64) -> Option<i64> {
        let t = Self::tix(tau_ns)?;
        if self.n_days == 0 {
            return None;
        }
        let x = self.day_x[slot(self.n_days - 1)][t];
        if x == NONE {
            None
        } else {
            Some(x)
        }
    }

    /// `(a_1e9, b_1e9)` of the tenor's rolling fit; `None` below
    /// [`crate::MIN_PAIRS`] pairs or on a degenerate ring.
    #[must_use]
    pub fn fit(&self, tau_ns: u64) -> Option<(i64, i64)> {
        let t = Self::tix(tau_ns)?;
        if self.fitted[t] {
            Some((self.a_1e9[t], self.b_1e9[t]))
        } else {
            None
        }
    }

    /// The fitted forecast `a + b·x/1e9` ×1e9 (log domain).
    #[must_use]
    pub fn ln_sigma_fit_1e9(&self, tau_ns: u64) -> Option<i64> {
        let x = self.x_1e9(tau_ns)?;
        let (a, b) = self.fit(tau_ns)?;
        Some(a + Self::slope_term(b, x))
    }

    /// A forecast as an ANNUALISED fraction ×1e9 — `VolEngine`'s
    /// annualising law with the tenor's own annualiser.
    #[must_use]
    pub fn sigma_ann_1e9(&self, tau_ns: u64, which: LongForecast) -> Option<i64> {
        let tenor = long_tenor_of(tau_ns)?;
        let ln = match which {
            LongForecast::Raw => self.x_1e9(tau_ns)?,
            LongForecast::Fit => self.ln_sigma_fit_1e9(tau_ns)?,
        };
        VolEngine::annualised_1e9(ln, tenor.annualise_1e9)
    }

    /// Pairs held for the tenor (0 off the grid).
    #[must_use]
    pub fn n_pairs(&self, tau_ns: u64) -> usize {
        match Self::tix(tau_ns) {
            Some(t) => self.n_pairs[t],
            None => 0,
        }
    }

    /// The tenor's QLIKE tell (all zero off the grid or before a score).
    #[must_use]
    pub fn qlike_counters(&self, tau_ns: u64) -> LongQlike {
        let Some(t) = Self::tix(tau_ns) else {
            return LongQlike::default();
        };
        let n = self.q_n[t];
        if n == 0 {
            return LongQlike::default();
        }
        let (raw, fit) = trailing_means_1e9(&self.q_raw_1e9[t][..n], &self.q_fit_1e9[t][..n]);
        LongQlike {
            n: n as u32,
            raw_mean_1e9: raw,
            fit_mean_1e9: fit,
            fit_beats_raw: n == QLIKE_RING && fit < raw,
        }
    }

    // -----------------------------------------------------------------
    // The weekday profile (HAR H3.3 — the ruling "publish a profile")
    // -----------------------------------------------------------------

    /// The day clock's shape over the resident closed days: per UTC
    /// weekday ([`weekday_of`], Monday = 0), the mean `Σ r²` of its
    /// OBSERVED days over the mean of every observed day, ×1e6 — `1e6` is
    /// an average day, a closed-market Saturday of an equity perp sits far
    /// below it — and the observed days each mean is over (`0`: none, and
    /// the ratio is `0`). A description, never an input: the fold and the
    /// pairs are untouched by it (the equities' weekends are in their
    /// `Σ r²` as the `xyz` market trades them).
    ///
    /// The integer law (the Python mirror's, bit for bit): `m_w = ⌊S_w /
    /// N_w⌋`, `m = ⌊S / N⌋`, `p_w = ⌊m_w · 1e6 / m⌋`, every term
    /// non-negative; a product past `i128` saturates to `i64::MAX`, and a
    /// zero `m` (no observed day, or every one flat) leaves every ratio 0.
    #[must_use]
    pub fn weekday_profile_1e6(&self) -> ([i64; WEEKDAYS], [u32; WEEKDAYS]) {
        let mut sum = [0i128; WEEKDAYS];
        let mut cnt = [0u32; WEEKDAYS];
        let mut total: i128 = 0;
        let mut n_obs: u32 = 0;
        let n = self.n_resident();
        let mut i = 0usize;
        while i < n {
            let s = slot(self.n_days - n as u64 + i as u64);
            if self.day_n[s] > 0 {
                let w = weekday_of(self.day_ts_ms[s]);
                sum[w] = sum[w].saturating_add(self.day_sq[s]);
                cnt[w] += 1;
                total = total.saturating_add(self.day_sq[s]);
                n_obs += 1;
            }
            i += 1;
        }
        let mut out = [0i64; WEEKDAYS];
        if n_obs == 0 {
            return (out, cnt);
        }
        let mean = total / n_obs as i128;
        if mean <= 0 {
            return (out, cnt);
        }
        let mut w = 0usize;
        while w < WEEKDAYS {
            if cnt[w] > 0 {
                let mw = sum[w] / cnt[w] as i128;
                out[w] = match mw.checked_mul(1_000_000) {
                    Some(v) => {
                        let r = v / mean;
                        if r > i64::MAX as i128 {
                            i64::MAX
                        } else {
                            r as i64
                        }
                    }
                    None => i64::MAX,
                };
            }
            w += 1;
        }
        (out, cnt)
    }

    // -----------------------------------------------------------------
    // The writer's view (restore is exactly its inverse)
    // -----------------------------------------------------------------

    /// Closed days resident, at most [`DAY_RING`].
    #[must_use]
    pub const fn n_resident(&self) -> usize {
        if self.n_days < DAY_RING as u64 {
            self.n_days as usize
        } else {
            DAY_RING
        }
    }

    /// Global index of the `i`-th resident day, oldest first.
    #[inline]
    fn resident_g(&self, i: usize) -> Option<u64> {
        let n = self.n_resident();
        if i >= n {
            None
        } else {
            Some(self.n_days - n as u64 + i as u64)
        }
    }

    /// The `i`-th resident closed day, oldest first:
    /// `(day_ts_ms, sum_sq, n_min)`.
    #[must_use]
    pub fn day_at(&self, i: usize) -> Option<(u64, i128, u32)> {
        let s = slot(self.resident_g(i)?);
        Some((self.day_ts_ms[s], self.day_sq[s], self.day_n[s]))
    }

    /// The open day: `(day_ts_ms, sum_sq, n_min)`; `None` before the
    /// first close.
    #[must_use]
    pub const fn open_day(&self) -> Option<(u64, i128, u32)> {
        if self.cur_day_ts_ms == 0 {
            None
        } else {
            Some((self.cur_day_ts_ms, self.cur_sq, self.cur_n))
        }
    }

    /// The arm tenor `tau_ns` made at the close of the `i`-th resident
    /// day: `(x_1e9, fit_1e9)`, the fit `i64::MIN` where none existed
    /// then. `None` where that close armed nothing.
    #[must_use]
    pub fn arm_at(&self, i: usize, tau_ns: u64) -> Option<(i64, i64)> {
        let t = Self::tix(tau_ns)?;
        let s = slot(self.resident_g(i)?);
        let x = self.day_x[s][t];
        if x == NONE {
            None
        } else {
            Some((x, self.day_fit[s][t]))
        }
    }

    /// The tenor's `i`-th pair, oldest first:
    /// `(target_first_day_ts_ms, x_1e9, y_1e9)`.
    #[must_use]
    pub fn pair_at(&self, tau_ns: u64, i: usize) -> Option<(u64, i64, i64)> {
        let t = Self::tix(tau_ns)?;
        if i >= self.n_pairs[t] {
            return None;
        }
        let k = VolEngine::chrono(self.pair_head[t], self.n_pairs[t], PAIR_RING_LONG, i);
        Some((
            self.pair_ts_ms[t][k],
            self.pair_x_1e9[t][k],
            self.pair_y_1e9[t][k],
        ))
    }

    /// The tenor's `i`-th QLIKE row, oldest first: `(raw_1e9, fit_1e9)`.
    #[must_use]
    pub fn qlike_at(&self, tau_ns: u64, i: usize) -> Option<(i64, i64)> {
        let t = Self::tix(tau_ns)?;
        if i >= self.q_n[t] {
            return None;
        }
        let k = VolEngine::chrono(self.q_head[t], self.q_n[t], QLIKE_RING, i);
        Some((self.q_raw_1e9[t][k], self.q_fit_1e9[t][k]))
    }

    // -----------------------------------------------------------------
    // Restore (boot only; each is refused, never repaired)
    // -----------------------------------------------------------------

    /// Restore one CLOSED day, oldest first, before any open day. It must
    /// be the calendar day after the newest one (the writer's ring is
    /// contiguous; a reader that merges sources fills its own holes with
    /// empty days). The day arms nothing until [`Self::seed_arm`] says so.
    pub fn seed_day(&mut self, day_ts_ms: u64, sum_sq: i128, n_min: u32) -> bool {
        let contiguous = self.n_days == 0 || day_ts_ms == self.last_day_ts() + DAY_MS;
        if self.cur_day_ts_ms != 0
            || day_ts_ms == 0
            || day_ts_ms % DAY_MS != 0
            || !(0..=SUM_SQ_MAX).contains(&sum_sq)
            || !contiguous
        {
            return false;
        }
        let s = slot(self.n_days);
        self.day_ts_ms[s] = day_ts_ms;
        self.day_sq[s] = sum_sq;
        self.day_n[s] = n_min;
        self.day_x[s] = [NONE; LONG_TAU_DAYS_MAX];
        self.day_fit[s] = [NONE; LONG_TAU_DAYS_MAX];
        self.n_days += 1;
        true
    }

    /// Restore the OPEN day and the minute feed's position — the close
    /// the next return is formed against, so the first live close after
    /// a restart forms a return across the hole (counted as a gap) rather
    /// than only priming. The open day follows the newest closed one.
    pub fn seed_open(
        &mut self,
        day_ts_ms: u64,
        sum_sq: i128,
        n_min: u32,
        last_min_ts_ms: u64,
        prev_px_1e6: i64,
    ) -> bool {
        let contiguous = self.n_days == 0 || day_ts_ms == self.last_day_ts() + DAY_MS;
        if self.cur_day_ts_ms != 0
            || day_ts_ms == 0
            || day_ts_ms % DAY_MS != 0
            || !(0..=SUM_SQ_MAX).contains(&sum_sq)
            || prev_px_1e6 <= 0
            || !(day_ts_ms..day_ts_ms + DAY_MS).contains(&last_min_ts_ms)
            || !contiguous
        {
            return false;
        }
        self.cur_day_ts_ms = day_ts_ms;
        self.cur_sq = sum_sq;
        self.cur_n = n_min;
        self.last_min_ts_ms = last_min_ts_ms;
        self.prev_px_1e6 = prev_px_1e6;
        true
    }

    /// Restore the arm a tenor made at the close of the resident day
    /// `day_ts_ms` (`fit_1e9 = i64::MIN`: no fit existed then).
    pub fn seed_arm(&mut self, tau_ns: u64, day_ts_ms: u64, x_1e9: i64, fit_1e9: i64) -> bool {
        let Some(t) = Self::tix(tau_ns) else {
            return false;
        };
        if !(0..=LN_ABS_MAX_1E9).contains(&x_1e9)
            || (fit_1e9 != NONE && !(-LN_ABS_MAX_1E9..=LN_ABS_MAX_1E9).contains(&fit_1e9))
        {
            return false;
        }
        let mut i = 0usize;
        while let Some(g) = self.resident_g(i) {
            let s = slot(g);
            if self.day_ts_ms[s] == day_ts_ms {
                self.day_x[s][t] = x_1e9;
                self.day_fit[s][t] = fit_1e9;
                return true;
            }
            i += 1;
        }
        false
    }

    /// Restore one pair of a tenor, oldest first. The tenor holds no fit
    /// until [`Self::refresh`].
    pub fn seed_pair(&mut self, tau_ns: u64, target_ts_ms: u64, x_1e9: i64, y_1e9: i64) -> bool {
        let Some(t) = Self::tix(tau_ns) else {
            return false;
        };
        if !(0..=LN_ABS_MAX_1E9).contains(&x_1e9) || !(0..=LN_ABS_MAX_1E9).contains(&y_1e9) {
            return false;
        }
        self.push_pair(t, target_ts_ms, x_1e9, y_1e9);
        self.fitted[t] = false;
        self.dirty = true;
        true
    }

    /// Restore one QLIKE row of a tenor, oldest first.
    pub fn seed_qlike(&mut self, tau_ns: u64, raw_1e9: i64, fit_1e9: i64) -> bool {
        let Some(t) = Self::tix(tau_ns) else {
            return false;
        };
        if raw_1e9 == NONE || fit_1e9 == NONE {
            return false;
        }
        self.push_qlike(t, raw_1e9, fit_1e9);
        true
    }

    /// Refit every tenor from its pair ring — once, after a restore. The
    /// first live minute calls it if the owner did not.
    pub fn refresh(&mut self) {
        let mut t = 0usize;
        while t < LONG_TAU_DAYS_MAX {
            self.refit(t);
            t += 1;
        }
        self.dirty = false;
    }
}

/// Ring slot of global day `g`.
#[inline]
const fn slot(g: u64) -> usize {
    (g % DAY_RING as u64) as usize
}

impl core::fmt::Debug for LongVolEngine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LongVolEngine")
            .field("n_days", &self.n_days)
            .field("cur_day_ts_ms", &self.cur_day_ts_ms)
            .field("cur_n", &self.cur_n)
            .field("last_min_ts_ms", &self.last_min_ts_ms)
            .field("gaps", &self.gaps)
            .field("refused", &self.refused)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
