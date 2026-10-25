// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-time
//!
//! Monotonic nanosecond clock. A thin wrapper over the platform syscall
//! that returns a `u64` — the type every POD struct uses as a timestamp.
//!
//! ## Why not `std::time::Instant`?
//!
//! `Instant` is correct but NOT `#[repr(C)]` and NOT a plain `u64`. Its
//! internal representation is platform-private. Since our ring slots
//! (`Tick`, `Signal`, `Fill`, `Order`) live in `#[repr(C, align(64))]`
//! structs, we want a POD timestamp type we fully control.
//!
//! ## Rules
//!
//! * `now_ns()` is inlined aggressively and allocates nothing.
//! * On Unix we call `clock_gettime(CLOCK_MONOTONIC_RAW)` directly to
//!   avoid NTP adjustment. `CLOCK_MONOTONIC_RAW` is available on both
//!   macOS (since 10.12) and Linux.
//! * Fallback (non-unix, tests) uses `std::time::Instant`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

/// Monotonic nanoseconds since some unspecified starting point. Values
/// are comparable across calls on the same process/run.
pub type NsTs = u64;

// ---------------------------------------------------------------
// Unix fast path.
// ---------------------------------------------------------------

/// Read the monotonic nanosecond clock. Zero allocations, inline-always.
///
/// On Linux and macOS this calls `clock_gettime(CLOCK_MONOTONIC_RAW)`
/// directly. On other unixes it falls back to `CLOCK_MONOTONIC`. On
/// non-unix targets (test-only scaffolding) it wraps `Instant`.
#[cfg(unix)]
#[inline(always)]
pub fn now_ns() -> NsTs {
    let mut ts = ::core::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `libc::clock_gettime` with a valid `timespec` pointer is
    // safe. `ts` is stack-allocated and fully owned here. We ignore the
    // return code and trust the monotonic clock to always be available
    // on macOS / Linux — if it ever isn't, the whole process is wedged.
    // `assume_init` is valid because a successful `clock_gettime` fully
    // writes both `tv_sec` and `tv_nsec`.
    unsafe {
        libc::clock_gettime(CLOCK_ID, ts.as_mut_ptr());
        let ts = ts.assume_init();
        (ts.tv_sec as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(ts.tv_nsec as u64)
    }
}

#[cfg(all(unix, target_os = "linux"))]
const CLOCK_ID: libc::clockid_t = libc::CLOCK_MONOTONIC_RAW;

#[cfg(all(unix, target_os = "macos"))]
const CLOCK_ID: libc::clockid_t = libc::CLOCK_MONOTONIC_RAW;

// BSDs / other unixes: plain CLOCK_MONOTONIC.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
const CLOCK_ID: libc::clockid_t = libc::CLOCK_MONOTONIC;

// ---------------------------------------------------------------
// Non-unix fallback (tests only).
// ---------------------------------------------------------------

/// Non-unix fallback. Tests and exotic targets only.
#[cfg(not(unix))]
pub fn now_ns() -> NsTs {
    // This branch is tested-only scaffolding; the production target is
    // unix. Using `Instant` here allocates nothing (it's a value type).
    use std::sync::OnceLock;
    use std::time::Instant;
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = BASE.get_or_init(Instant::now);
    base.elapsed().as_nanos() as u64
}

// ---------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------

/// Difference between two timestamps in nanoseconds. Saturates at zero
/// when `later < earlier` (time went backwards — should not happen with
/// `CLOCK_MONOTONIC_RAW`, but we guard against bugs rather than trust).
#[inline(always)]
pub const fn ns_since(earlier: NsTs, later: NsTs) -> u64 {
    later.saturating_sub(earlier)
}

// ---------------------------------------------------------------
// VT2: per-connection venue-clock offset + staleness judge.
// ---------------------------------------------------------------

/// Verdict of one [`FeedClock::judge`] call.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct FeedJudgement {
    /// How far this message trailed the connection's least-delayed
    /// message, in ms (≥ 0; saturates at `u32::MAX`). 0 when the venue
    /// time was unknown.
    pub delay_ms: u32,
    /// `delay_ms > stale_after_ms`. Always false for an unknown venue
    /// time (the v2 law: unknown is never stale).
    pub stale: bool,
}

/// Per-connection venue-time offset estimator + staleness judge
/// (docs/venue-time-capture-plan.md §2 doctrine 2).
///
/// Age is measured against the venue's own fastest message, never the
/// host wall clock: `off_ms = max over the window of (venue_ms −
/// mono_ms)`; every message's delay is `off_ms − (venue_ms − mono_ms)
/// ≥ 0`. `off_ms` decays by [`FeedClock::DECAY_MS_PER_MIN`] so a venue
/// clock step re-learns (a 100 ms backward step clears in 100 min —
/// the plan's accepted v1 bound). One estimator per CONNECTION: a
/// reconnect calls [`FeedClock::reset`].
///
/// Cost per message: one subtraction, two compares, one max, one
/// integer EMA step. No allocation, no syscall (the caller passes the
/// `now_ns()` it already took for the tick's `ts_ns`). POD, `Copy`.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct FeedClock {
    /// `max(venue_ms − mono_ms)` seen since the last reset, minus the
    /// decay; [`FeedClock::UNLEARNED`] until the first stamped message.
    off_ms: i64,
    /// Mono ms of the last decay step (aligned to whole minutes).
    decay_at_ms: i64,
    /// Staleness threshold (venue default or operator override).
    stale_after_ms: i64,
    /// Integer EMA of `delay_ms` ×16 (α = 1/16) — the cheap gauge the
    /// metrics page exports as `feed_delay_ema_ms`.
    delay_ema_x16: i64,
}

impl FeedClock {
    /// `off_ms` sentinel before the first stamped message.
    pub const UNLEARNED: i64 = i64::MIN;
    /// Slow decay of the learned offset (doctrine 2).
    pub const DECAY_MS_PER_MIN: i64 = 1;
    const MS_PER_MIN: i64 = 60_000;
    const EMA_SHIFT: u32 = 4;

    /// Fresh, unlearned estimator with the given staleness threshold
    /// (0 = measure only, never flag).
    #[inline(always)]
    pub const fn new(stale_after_ms: u32) -> Self {
        Self {
            off_ms: Self::UNLEARNED,
            decay_at_ms: 0,
            stale_after_ms: stale_after_ms as i64,
            delay_ema_x16: 0,
        }
    }

    /// Forget the learned offset (new connection = new offset). The
    /// threshold and the delay EMA survive.
    #[inline(always)]
    pub fn reset(&mut self) {
        self.off_ms = Self::UNLEARNED;
        self.decay_at_ms = 0;
    }

    /// True once a stamped message has set the offset.
    #[inline(always)]
    pub const fn learned(&self) -> bool {
        self.off_ms != Self::UNLEARNED
    }

    /// The staleness threshold this estimator judges against.
    #[inline(always)]
    pub const fn stale_after_ms(&self) -> u32 {
        self.stale_after_ms as u32
    }

    /// Smoothed delay in ms (EMA, α = 1/16).
    #[inline(always)]
    pub const fn delay_ema_ms(&self) -> u32 {
        (self.delay_ema_x16 >> Self::EMA_SHIFT) as u32
    }

    /// Judge one message stamped `venue_time_ms` (0 = unknown) that the
    /// ingress finished parsing at `mono_ns` (the tick's `ts_ns`).
    #[inline(always)]
    pub fn judge(&mut self, venue_time_ms: u64, mono_ns: NsTs) -> FeedJudgement {
        if venue_time_ms == 0 {
            return FeedJudgement {
                delay_ms: 0,
                stale: false,
            };
        }
        let mono_ms = (mono_ns / 1_000_000) as i64;
        // A garbage wire stamp above i64::MAX clamps instead of wrapping
        // negative; every step below saturates so a fuzzed stamp can
        // never overflow (no panics in release, none in debug either).
        let venue_ms = if venue_time_ms > i64::MAX as u64 { i64::MAX } else { venue_time_ms as i64 };
        let raw = venue_ms.saturating_sub(mono_ms);
        if self.off_ms == Self::UNLEARNED {
            self.off_ms = raw;
            self.decay_at_ms = mono_ms;
        } else {
            let elapsed_min = mono_ms.saturating_sub(self.decay_at_ms) / Self::MS_PER_MIN;
            // Branch taken once a minute; the decay is bounded by design.
            if elapsed_min > 0 {
                self.off_ms = self.off_ms.saturating_sub(elapsed_min * Self::DECAY_MS_PER_MIN);
                self.decay_at_ms = self.decay_at_ms.saturating_add(elapsed_min * Self::MS_PER_MIN);
            }
            if raw > self.off_ms {
                self.off_ms = raw;
            }
        }
        let delay = self.off_ms.saturating_sub(raw);
        debug_assert!(delay >= 0, "delay is max-relative and cannot be negative");
        self.delay_ema_x16 = self
            .delay_ema_x16
            .saturating_add(delay - (self.delay_ema_x16 >> Self::EMA_SHIFT));
        FeedJudgement {
            delay_ms: if delay > u32::MAX as i64 { u32::MAX } else { delay as u32 },
            // A zero threshold DISABLES the judgement (operator
            // `--stale-after-ms <venue>:0`): the delay is still measured
            // and exported, nothing is ever flagged.
            stale: self.stale_after_ms > 0 && delay > self.stale_after_ms,
        }
    }
}

// ---------------------------------------------------------------
// I2 (ICDP): wall anchor + bar clock.
// ---------------------------------------------------------------

/// One (monotonic, wall) pair taken back-to-back ONCE at boot, so a
/// strategy can put a monotonic timestamp on a UTC bar grid without
/// ever touching the wall clock again (doctrine: venue time is data,
/// the monotonic clock orders, the wall clock only ANCHORS).
///
/// Live: [`WallAnchor::now`] in the boot path. Replay: the harness
/// builds it from its virtual-clock rebase (`virt = VIRT_T0 + (epoch −
/// epoch_0) + (ts − ts_first)`, `wall = epoch + (ts − ts_first)`), i.e.
/// `WallAnchor::new(VIRT_T0, epoch_0)` — bars fall on the same UTC
/// boundaries offline as they did live. POD, `Copy`, no drop.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct WallAnchor {
    /// Monotonic ns at the anchor instant.
    pub mono_ns: NsTs,
    /// Wall-clock ns since the Unix epoch at the same instant.
    pub wall_ns: u64,
}

impl WallAnchor {
    /// Pin the anchor.
    #[inline(always)]
    pub const fn new(mono_ns: NsTs, wall_ns: u64) -> Self {
        Self { mono_ns, wall_ns }
    }

    /// Take the anchor NOW (two syscalls back-to-back; boot path only —
    /// never called from a tick callback).
    pub fn now() -> Self {
        let mono_ns = now_ns();
        let wall_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self { mono_ns, wall_ns }
    }

    /// Wall ns of a monotonic instant. Wrapping arithmetic on purpose:
    /// an instant slightly BEFORE the anchor (a tick parsed in the boot
    /// race) maps to `wall − (anchor − instant)` exactly, and the
    /// caller guarantees `wall ≥ anchor − instant` (both are ≥ 2026).
    #[inline(always)]
    pub const fn wall_of(&self, mono_ns: NsTs) -> u64 {
        self.wall_ns
            .wrapping_add(mono_ns.wrapping_sub(self.mono_ns))
    }

    /// Monotonic ns of a wall instant (the inverse of [`Self::wall_of`],
    /// same wrapping law).
    #[inline(always)]
    pub const fn mono_of(&self, wall_ns: u64) -> NsTs {
        self.mono_ns
            .wrapping_add(wall_ns.wrapping_sub(self.wall_ns))
    }
}

/// A UTC-aligned bar grid over the monotonic clock: bar `id` opens at
/// wall `id × tf` (so `tf = 60 s` bars open on the minute), the
/// decision point sits `delta` after the open, the close is the next
/// open. Pure integer arithmetic, inlined; the only division is in
/// [`BarClock::bar_id`], which a strategy calls on a ROLL, not per
/// tick (compare against [`BarClock::close_mono`] instead). POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct BarClock {
    /// The boot / replay anchor.
    pub anchor: WallAnchor,
    /// Bar length, ns (> 0).
    pub tf_ns: u64,
    /// Decision offset from the open, ns (< `tf_ns`).
    pub delta_ns: u64,
}

impl BarClock {
    /// Build a grid. `tf_ns > 0`, `delta_ns < tf_ns` (debug-asserted;
    /// a config layer validates before boot).
    #[inline(always)]
    pub const fn new(anchor: WallAnchor, tf_ns: u64, delta_ns: u64) -> Self {
        debug_assert!(tf_ns > 0, "bar length must be positive");
        debug_assert!(delta_ns < tf_ns, "decision must fall inside the bar");
        Self {
            anchor,
            tf_ns,
            delta_ns,
        }
    }

    /// The bar containing monotonic instant `mono_ns` (one division).
    #[inline(always)]
    pub const fn bar_id(&self, mono_ns: NsTs) -> u64 {
        self.anchor.wall_of(mono_ns) / self.tf_ns
    }

    /// Wall ns at which bar `id` opens.
    #[inline(always)]
    pub const fn open_wall(&self, id: u64) -> u64 {
        id * self.tf_ns
    }

    /// Monotonic ns at which bar `id` opens.
    #[inline(always)]
    pub const fn open_mono(&self, id: u64) -> NsTs {
        self.anchor.mono_of(self.open_wall(id))
    }

    /// Monotonic ns of bar `id`'s decision point (`open + delta`).
    #[inline(always)]
    pub const fn decision_mono(&self, id: u64) -> NsTs {
        self.open_mono(id).wrapping_add(self.delta_ns)
    }

    /// Monotonic ns at which bar `id` closes (= the next open).
    #[inline(always)]
    pub const fn close_mono(&self, id: u64) -> NsTs {
        self.open_mono(id).wrapping_add(self.tf_ns)
    }

    /// Time left in bar `id` at `mono_ns` — the `Order.ttl_ns` of an
    /// intent that must not outlive its bar (0 when the bar is over).
    #[inline(always)]
    pub const fn ttl_to_close(&self, id: u64, mono_ns: NsTs) -> u64 {
        self.close_mono(id).saturating_sub(mono_ns)
    }
}

// ---------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------

// ---------------------------------------------------------------
// T2T T1-a/b: the hot window — a recurring wall-clock window in which a
// thread never blocks.
// ---------------------------------------------------------------

/// Wall-clock ms since the Unix epoch; 0 before it. (`SystemTime`: one
/// vDSO / commpage read, no syscall on Linux or macOS.)
#[inline]
#[must_use]
pub fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// **T2T T1-a/b — a recurring wall-clock window in which a thread never
/// blocks.** A member whose orders cluster before fixed expiries (BIN15's
/// 15 m families fire at τ < 30 s) keeps its gateway and its ingress awake
/// through that window: work then waits for one non-blocking poll, never
/// for a park or a kernel wake-up. Measured 2026-10-25 on the Mac (the t2t
/// harness): a parked gateway takes 159 µs p50 from push to the socket
/// write, an awake one 42 µs; a thread woken from `poll` by one loopback
/// byte 94 µs p50 / 184 µs p99, a spinning one 65 / 113 µs.
///
/// Hot when `wall_ms mod period` lies in `[period − open, period − close)`:
/// from `open` ms before each period boundary until `close` ms before it.
/// 8 B, `Copy`; packs into one word ([`HotCell`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HotWindow {
    period_ms: u32,
    open_ms: u16,
    close_ms: u16,
}

impl HotWindow {
    /// No window: never hot.
    pub const OFF: Self = Self {
        period_ms: 0,
        open_ms: 0,
        close_ms: 0,
    };

    /// `None` unless `close_ms < open_ms ≤ period_ms`, `open_ms ≤ u16::MAX`
    /// (a window opens at most ~65 s before its boundary) and
    /// `0 < period_ms ≤ u32::MAX` (~49 days).
    #[must_use]
    pub const fn new(period_ms: u64, open_ms: u64, close_ms: u64) -> Option<Self> {
        if period_ms == 0
            || period_ms > u32::MAX as u64
            || open_ms > period_ms
            || open_ms > u16::MAX as u64
            || close_ms >= open_ms
        {
            return None;
        }
        Some(Self {
            period_ms: period_ms as u32,
            open_ms: open_ms as u16,
            close_ms: close_ms as u16,
        })
    }

    /// Whether `wall_ms` (Unix ms) is inside the window; never for
    /// [`Self::OFF`].
    #[inline]
    #[must_use]
    pub const fn contains(&self, wall_ms: u64) -> bool {
        if self.period_ms == 0 {
            return false;
        }
        let p = self.period_ms as u64;
        let r = wall_ms % p;
        r >= p - self.open_ms as u64 && r < p - self.close_ms as u64
    }

    /// Whether this is [`Self::OFF`].
    #[inline]
    #[must_use]
    pub const fn is_off(&self) -> bool {
        self.period_ms == 0
    }

    /// The share of the time the window is hot, in ppm — a spin's CPU cost
    /// on one core (0 for [`Self::OFF`]).
    #[must_use]
    pub const fn duty_ppm(&self) -> u64 {
        if self.period_ms == 0 {
            return 0;
        }
        (self.open_ms - self.close_ms) as u64 * 1_000_000 / self.period_ms as u64
    }

    /// The window as one word, for [`HotCell`].
    #[inline]
    #[must_use]
    pub const fn to_word(self) -> u64 {
        ((self.period_ms as u64) << 32) | ((self.open_ms as u64) << 16) | self.close_ms as u64
    }

    /// The inverse of [`Self::to_word`].
    #[inline]
    #[must_use]
    pub const fn from_word(w: u64) -> Self {
        Self {
            period_ms: (w >> 32) as u32,
            open_ms: (w >> 16) as u16,
            close_ms: w as u16,
        }
    }
}

/// **A hot window one thread publishes and another reads** (T2T T1-b): the
/// boot sets it once a member's window is known — after the ingress it
/// serves has started — and the ingress reads it every poll. One relaxed
/// word; [`HotWindow::OFF`] until set.
#[derive(Debug, Default)]
pub struct HotCell(std::sync::atomic::AtomicU64);

impl HotCell {
    /// [`HotWindow::OFF`].
    #[must_use]
    pub const fn new() -> Self {
        Self(std::sync::atomic::AtomicU64::new(0))
    }

    /// Publish `w`.
    #[inline]
    pub fn set(&self, w: HotWindow) {
        self.0.store(w.to_word(), std::sync::atomic::Ordering::Relaxed);
    }

    /// The window in force.
    #[inline]
    #[must_use]
    pub fn get(&self) -> HotWindow {
        HotWindow::from_word(self.0.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Whether now is inside the window in force (no clock read when off).
    #[inline]
    #[must_use]
    pub fn is_hot_now(&self) -> bool {
        let w = self.get();
        !w.is_off() && w.contains(wall_ms())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hot_window_opens_before_each_boundary_and_closes_before_it() {
        // BIN15's: 15 min, from 32 s before each quarter hour to 1 s before.
        let w = HotWindow::new(900_000, 32_000, 1_000).expect("valid");
        let q = 1_792_900_800_000u64; // a quarter-hour boundary (Unix ms)
        assert_eq!(q % 900_000, 0);
        assert!(!w.contains(q - 32_001), "before the window");
        assert!(w.contains(q - 32_000), "its first ms");
        assert!(w.contains(q - 1_001), "its last ms");
        assert!(!w.contains(q - 1_000), "closed 1 s before the boundary");
        assert!(!w.contains(q), "the boundary");
        assert!(!w.contains(q + 400_000), "mid-period");
        assert_eq!(w.duty_ppm(), 34_444, "31 s of 900");
        assert_eq!(HotWindow::OFF.duty_ppm(), 0);
        assert!(!HotWindow::OFF.contains(q - 10_000));
        assert_eq!(HotWindow::from_word(w.to_word()), w, "the word round-trips");
    }

    #[test]
    fn a_hot_window_must_open_before_it_closes_inside_its_period() {
        assert!(HotWindow::new(0, 1, 0).is_none());
        assert!(HotWindow::new(900_000, 900_001, 0).is_none());
        assert!(HotWindow::new(900_000, 1_000, 1_000).is_none());
        assert!(HotWindow::new(60_000, 60_000, 0).is_some(), "always hot");
        assert!(HotWindow::new(900_000, 70_000, 0).is_none(), "opens past u16 ms");
        assert!(HotWindow::new(u64::from(u32::MAX) + 1, 2, 1).is_none(), "past u32");
    }

    #[test]
    fn a_hot_cell_is_off_until_set_and_then_reads_the_window() {
        let c = HotCell::new();
        assert!(c.get().is_off());
        assert!(!c.is_hot_now());
        let always = HotWindow::new(60_000, 60_000, 0).expect("always hot");
        c.set(always);
        assert_eq!(c.get(), always);
        assert!(c.is_hot_now());
        c.set(HotWindow::OFF);
        assert!(!c.is_hot_now());
    }

    const MS: u64 = 1_000_000;

    // -------- I2: WallAnchor + BarClock --------

    const WALL_2026: u64 = 1_788_400_000_000_000_000; // 2026-09-03T02:26:40Z
    const MONO_0: u64 = 3_191_000_000_000_000;

    #[test]
    fn wall_anchor_maps_both_ways_and_survives_a_pre_anchor_instant() {
        let a = WallAnchor::new(MONO_0, WALL_2026);
        assert_eq!(a.wall_of(MONO_0), WALL_2026);
        assert_eq!(a.wall_of(MONO_0 + 5 * MS), WALL_2026 + 5 * MS);
        assert_eq!(a.mono_of(WALL_2026 + 5 * MS), MONO_0 + 5 * MS);
        // 7 ns before the anchor: wraps back exactly, no saturation.
        assert_eq!(a.wall_of(MONO_0 - 7), WALL_2026 - 7);
        assert_eq!(a.mono_of(a.wall_of(12_345)), 12_345);
    }

    #[test]
    fn wall_anchor_now_is_within_a_second_of_the_system_clock() {
        let a = WallAnchor::now();
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        assert!(a.wall_ns > 1_700_000_000_000_000_000, "epoch ns, not zero");
        assert!(wall.abs_diff(a.wall_ns) < 1_000_000_000);
        assert!(now_ns() >= a.mono_ns);
    }

    #[test]
    fn bar_ids_are_utc_aligned_and_monotone_with_open_decision_close_in_order() {
        let a = WallAnchor::new(MONO_0, WALL_2026);
        let tf = 15_000 * MS; // 15 s
        let c = BarClock::new(a, tf, 3_750 * MS); // δ = 25 %
        // The anchor instant sits inside some bar; its open is on the
        // 15 s wall grid.
        let id0 = c.bar_id(MONO_0);
        assert_eq!(c.open_wall(id0) % tf, 0);
        assert!(c.open_wall(id0) <= WALL_2026 && WALL_2026 < c.open_wall(id0 + 1));
        assert_eq!(c.open_mono(id0) + tf, c.close_mono(id0));
        assert_eq!(c.close_mono(id0), c.open_mono(id0 + 1));
        assert_eq!(c.decision_mono(id0), c.open_mono(id0) + 3_750 * MS);
        // Monotone: the last ns of the bar is still the bar; the close is the next.
        assert_eq!(c.bar_id(c.close_mono(id0) - 1), id0);
        assert_eq!(c.bar_id(c.close_mono(id0)), id0 + 1);
        // 1 h later: 240 bars on.
        assert_eq!(c.bar_id(c.open_mono(id0) + 3_600_000 * MS), id0 + 240);
    }

    #[test]
    fn ttl_to_close_counts_down_and_floors_at_zero() {
        let a = WallAnchor::new(MONO_0, WALL_2026);
        let c = BarClock::new(a, 60_000 * MS, 30_000 * MS);
        let id = c.bar_id(MONO_0);
        let open = c.open_mono(id);
        assert_eq!(c.ttl_to_close(id, open), 60_000 * MS);
        assert_eq!(c.ttl_to_close(id, open + 59_999 * MS), MS);
        assert_eq!(c.ttl_to_close(id, c.close_mono(id)), 0);
        assert_eq!(c.ttl_to_close(id, c.close_mono(id) + 5), 0);
    }

    #[test]
    fn replay_anchor_reproduces_the_harness_rebase() {
        // Harness law: virt = VIRT_T0 + (epoch − epoch_0) + (ts − ts_first),
        // wall = epoch + (ts − ts_first)  ⇒  anchor = (VIRT_T0, epoch_0).
        const VIRT_T0: u64 = 100_000_000_000_000_000;
        let epoch_0 = WALL_2026;
        let a = WallAnchor::new(VIRT_T0, epoch_0);
        let epoch_1 = epoch_0 + 8 * 3_600_000 * MS; // a second run 8 h later
        let ts_first = 77_777;
        let ts = ts_first + 12_345 * MS;
        let virt = VIRT_T0 + (epoch_1 - epoch_0) + (ts - ts_first);
        let wall = epoch_1 + (ts - ts_first);
        assert_eq!(a.wall_of(virt), wall);
        let c = BarClock::new(a, 60_000 * MS, 15_000 * MS);
        assert_eq!(c.bar_id(virt), wall / (60_000 * MS));
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "decision must fall inside the bar")]
    fn bar_clock_refuses_a_decision_outside_the_bar() {
        let _ = BarClock::new(WallAnchor::new(0, 0), 1_000, 1_000);
    }

    #[test]
    fn feed_clock_unknown_venue_time_is_never_stale_and_never_learns() {
        let mut c = FeedClock::new(400);
        let j = c.judge(0, 5_000 * MS);
        assert_eq!(j, FeedJudgement { delay_ms: 0, stale: false });
        assert!(!c.learned());
        assert_eq!(c.delay_ema_ms(), 0);
    }

    #[test]
    fn feed_clock_first_message_defines_the_offset_with_zero_delay() {
        let mut c = FeedClock::new(400);
        // venue clock is 62 ms ahead of the host at the least-delayed message
        let j = c.judge(1_000_062, 1_000_000 * MS);
        assert_eq!(j.delay_ms, 0);
        assert!(!j.stale);
        assert!(c.learned());
        assert_eq!(c.stale_after_ms(), 400);
    }

    #[test]
    fn feed_clock_delay_is_relative_to_the_fastest_message_and_flags_stale() {
        let mut c = FeedClock::new(400);
        let _ = c.judge(1_000_062, 1_000_000 * MS); // off = +62
        // 300 ms later on the host, venue says only 10 ms passed: 290 ms behind
        let j = c.judge(1_000_072, 1_000_300 * MS);
        assert_eq!(j.delay_ms, 290);
        assert!(!j.stale);
        // a 5 s-old message: stale
        let j = c.judge(1_000_100, 1_005_100 * MS);
        assert_eq!(j.delay_ms, 5_062);
        assert!(j.stale);
        // a faster message raises the offset and is itself fresh
        let j = c.judge(1_005_170, 1_005_100 * MS); // raw = +70 > +62
        assert_eq!(j.delay_ms, 0);
        assert!(!j.stale);
        // and the previous stale one would now measure 8 ms worse
        let j = c.judge(1_000_100, 1_005_100 * MS);
        assert_eq!(j.delay_ms, 5_070);
    }

    #[test]
    fn feed_clock_threshold_is_strict_greater_than() {
        let mut c = FeedClock::new(400);
        let _ = c.judge(2_000_000, 2_000_000 * MS);
        assert!(!c.judge(2_000_000, 2_000_400 * MS).stale, "400 == threshold is fresh");
        assert!(c.judge(2_000_000, 2_000_401 * MS).stale, "401 > threshold is stale");
    }

    #[test]
    fn feed_clock_zero_threshold_measures_but_never_flags() {
        let mut c = FeedClock::new(0);
        let _ = c.judge(3_000_000, 3_000_000 * MS);
        let j = c.judge(3_000_000, 3_059_000 * MS); // 59 s behind, before any decay step
        assert_eq!(j.delay_ms, 59_000);
        assert!(!j.stale);
    }

    #[test]
    fn feed_clock_offset_decays_one_ms_per_minute_and_relearns() {
        let mut c = FeedClock::new(1_000);
        let _ = c.judge(10_000_000, 10_000_000 * MS); // off = 0
        // venue clock steps BACK 100 ms: every message now looks 100 ms late
        let j = c.judge(10_000_900, 10_001_000 * MS);
        assert_eq!(j.delay_ms, 100);
        // 30 minutes later the offset has decayed 30 ms: apparent delay 70
        let j = c.judge(11_800_900, 11_801_000 * MS);
        assert_eq!(j.delay_ms, 70);
        // 100+ minutes after the step it has fully re-learned
        let j = c.judge(16_000_900, 16_001_000 * MS);
        assert_eq!(j.delay_ms, 0);
    }

    #[test]
    fn feed_clock_reset_forgets_the_offset_but_keeps_the_threshold() {
        let mut c = FeedClock::new(700);
        let _ = c.judge(1_000, 1_000 * MS);
        assert!(c.learned());
        c.reset();
        assert!(!c.learned());
        assert_eq!(c.stale_after_ms(), 700);
        // the next message starts a fresh offset: zero delay by definition
        assert_eq!(c.judge(500, 1_000 * MS).delay_ms, 0);
    }

    #[test]
    fn feed_clock_delay_ema_tracks_a_constant_delay() {
        let mut c = FeedClock::new(400);
        let _ = c.judge(1_000, 1_000 * MS);
        for i in 1..200u64 {
            let _ = c.judge(1_000 + i, (1_000 + i + 50) * MS); // constant 50 ms behind
        }
        assert!((45..=50).contains(&c.delay_ema_ms()), "ema={}", c.delay_ema_ms());
    }

    #[test]
    fn feed_clock_delay_saturates_at_u32_max() {
        let mut c = FeedClock::new(400);
        let _ = c.judge(u64::MAX / 4, 0);
        let j = c.judge(1, u64::MAX / 2);
        assert_eq!(j.delay_ms, u32::MAX);
        assert!(j.stale);
    }

    #[test]
    fn now_ns_is_monotonic_across_two_calls() {
        let a = now_ns();
        let b = now_ns();
        assert!(b >= a, "a={a} b={b}");
    }

    #[test]
    fn ns_since_saturates_on_reverse() {
        assert_eq!(ns_since(100, 50), 0);
    }

    #[test]
    fn ns_since_computes_simple_delta() {
        assert_eq!(ns_since(1_000, 1_500), 500);
    }

    #[test]
    fn now_ns_advances_over_a_busy_loop() {
        // `CLOCK_MONOTONIC` has nanosecond resolution on Linux, but
        // actual tick rate is platform-dependent — on some ARM cores
        // (e.g. the Graviton-family used in CI) the backing counter
        // advances in ~40 ns steps, so a fixed 1024-iter spin can
        // race the counter. Spin until the clock actually advances,
        // bounded so we never wedge the suite.
        let a = now_ns();
        let mut spin = 0u64;
        let mut b = a;
        for i in 0..1_000_000u64 {
            spin = spin.wrapping_add(i);
            b = now_ns();
            if b > a {
                break;
            }
        }
        // Black-box the spin result so LLVM doesn't elide it.
        ::std::hint::black_box(spin);
        assert!(b > a, "a={a} b={b}");
    }
}
