// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! # backtest::clock — the harness's WALL clock, per run (BIN15 S2)
//!
//! Every capture stamp is the engine's MONOTONIC clock; the replay orders
//! records on it (the virtual clock, §3.3) and that order is untouched
//! here. What this module decides is the WALL instant a record is
//! reported and settled at — the clock a HIP-4 expiry, a Deribit
//! expiry, a UTC day and a member's `now` are compared on.
//!
//! **The old law** (every capture before this, and every v2 capture
//! still): `wall = epoch + (ts − ts_first)` — the run directory's epoch
//! pinned to the run's FIRST record. The directory is named at boot and
//! the first tick arrives 10–25 s later, so on this host that law runs
//! 23–42 s EARLY against the venue, and not by a constant (vault doc 27
//! §4: median 24.75 s). Against a 60-second settlement window ending at
//! `T` that is a different minute.
//!
//! **The venue law** (a run whose Hyperliquid ticks carry
//! `venue_time_ms`, i.e. every v3 capture with an HL lane):
//! `wall = ts + off`, `off = median(venue_time_ms·1e6 − ts)` over the
//! first [`VENUE_OFFSET_SAMPLES`] such ticks — the venue's own clock
//! against our stamp, accurate to the feed delay (≈ 0.1–0.3 s here). It
//! is `claude_worker.hip4.venue_wall_offset_ns`'s law (that one samples
//! 4 000 ticks across the file; on the 2026-09-13→23 captures the two
//! agree within 0.34 s on every run), in a fixed `[i64; 64]` with no
//! allocation.
//!
//! One mapping for `backtest`, `backtest --member` and `audit-pnl`
//! ([`RunClock::wall_of`]), so on a venue-clock run no two surfaces put
//! one record at two instants. (On the anchor law each surface still
//! pins ITS OWN first record — the backtest's includes events and depth,
//! the audit's is its first tick — a difference that predates S2.)
//!
//! **The fit can be refused.** The run directory is named at boot,
//! before the first record exists, so the venue's clock can never put
//! that record BEFORE the directory's epoch. A fit that does (by more
//! than [`EPOCH_SKEW_TOLERANCE_NS`] of clock skew) is dominated by stale
//! snapshots — more than half of the first stamped ticks — and the run
//! falls back to the anchor law, loudly ([`ClockTell::Refused`]).
//!
//! DOCTRINE (audit_replay.rs): offline — never loaded by the engine
//! loop; deterministic integer math; the fit and the mapping allocate
//! nothing (the tell renders through `Display` at report time).

use core_types::Tick;

/// Hyperliquid ticks with a venue stamp sampled per run for the offset.
pub const VENUE_OFFSET_SAMPLES: usize = 64;

/// How far before its directory's epoch a fit may put a run's first
/// record and still be believed, ns: the host's and the venue's clocks
/// are NTP-disciplined, so anything past this is a bad fit, not skew.
pub const EPOCH_SKEW_TOLERANCE_NS: i64 = 2_000_000_000;

/// The first [`VENUE_OFFSET_SAMPLES`] `venue_time_ms·1e6 − ts_ns` of a
/// run's Hyperliquid ticks, in file order. Fixed storage: no allocation.
/// `Clone` only (520 B): a copy of it should be spelled out.
#[derive(Clone, Debug)]
pub struct VenueOffsetFit {
    samples: [i64; VENUE_OFFSET_SAMPLES],
    n: usize,
    /// The capture stamp of the first sample — the refusal test's input,
    /// the same on every surface that feeds the same raw HL ticks.
    first_ts: u64,
}

impl Default for VenueOffsetFit {
    fn default() -> Self {
        Self::new()
    }
}

impl VenueOffsetFit {
    /// An empty fit.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            samples: [0; VENUE_OFFSET_SAMPLES],
            n: 0,
            first_ts: 0,
        }
    }

    /// Whether the fit holds all the samples it takes.
    #[inline]
    #[must_use]
    pub const fn full(&self) -> bool {
        self.n >= VENUE_OFFSET_SAMPLES
    }

    /// Fold one RAW captured tick (before any re-judge or remap). A tick
    /// with no venue stamp (`venue_time_ms == 0`: v2, or a lane that
    /// carries none) and every tick past the first
    /// [`VENUE_OFFSET_SAMPLES`] stamped ones are ignored.
    #[inline]
    pub fn observe(&mut self, t: &Tick) {
        if self.full() || t.venue_time_ms == 0 {
            return;
        }
        let venue_ns = i128::from(t.venue_time_ms) * 1_000_000;
        let d = venue_ns - i128::from(t.ts_ns);
        // A stamp whose difference leaves `i64` is not a clock (a year
        // ~2262 or a mangled field); it is dropped rather than wrapped.
        if let Ok(v) = i64::try_from(d) {
            if self.n == 0 {
                self.first_ts = t.ts_ns;
            }
            self.samples[self.n] = v;
            self.n += 1;
        }
    }

    /// `(offset, first sampled stamp)`, the pair [`RunClock::choose`]
    /// takes, or `None` with no sample.
    #[must_use]
    pub fn fit(&mut self) -> Option<(i64, u64)> {
        let first = self.first_ts;
        self.offset_ns().map(|o| (o, first))
    }

    /// The median of the samples, ns (the mean of the two middle ones on
    /// an even count, truncated toward zero), or `None` with no sample.
    /// Selects IN PLACE — the order of the samples carries nothing.
    #[must_use]
    pub fn offset_ns(&mut self) -> Option<i64> {
        if self.n == 0 {
            return None;
        }
        let n = self.n;
        let mid = n / 2;
        let (lower, hi, _) = self.samples[..n].select_nth_unstable(mid);
        let hi = *hi;
        if n % 2 == 1 {
            return Some(hi);
        }
        // Even count: the lower middle is the largest of the lower half.
        let mut lo = i64::MIN;
        let mut k = 0usize;
        while k < lower.len() {
            if lower[k] > lo {
                lo = lower[k];
            }
            k += 1;
        }
        i64::try_from((i128::from(lo) + i128::from(hi)) / 2).ok()
    }
}

/// One run's wall mapping.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunClock {
    /// The old law, kept bit for bit where the capture cannot say
    /// better: `wall = epoch + (ts − ts_first)`, a stamp before the
    /// anchor clamped to it.
    Anchor {
        /// The run directory's epoch, ns.
        epoch_ns: u64,
        /// The run's first record stamp (the §3.2 order's minimum).
        ts_first: u64,
    },
    /// The venue law: `wall = ts + offset_ns`.
    Venue {
        /// `venue_time − ts`, ns — the median over the run's first HL
        /// stamps ([`VenueOffsetFit`]).
        offset_ns: i64,
    },
}

/// What the summary says about a run's clock. Renders through `Display`
/// straight into the report; [`ClockTell::Silent`] renders nothing, so a
/// capture without venue stamps reports byte for byte as before.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClockTell {
    /// The anchor law: the capture carried no venue stamp.
    Silent,
    /// The venue law, with how far the anchor law was from it.
    Venue {
        /// The fitted offset, ns.
        offset_ns: i64,
        /// `offset − (epoch − ts_first)`: how EARLY the anchor law ran
        /// (negative = late), ns.
        anchor_early_ns: i64,
    },
    /// The fit put the first record before the directory's epoch: it was
    /// refused and the anchor law used.
    Refused {
        /// The refused offset, ns.
        offset_ns: i64,
        /// How far before the epoch it put the first record, ns.
        before_epoch_ns: i64,
    },
}

impl Default for ClockTell {
    fn default() -> Self {
        Self::Silent
    }
}

/// `s.mmm` of the magnitude of a signed ns amount (the caller words the
/// sign), so a value between −1 s and 0 cannot lose it.
fn write_secs(f: &mut core::fmt::Formatter<'_>, ns: i64) -> core::fmt::Result {
    let ms = ns.unsigned_abs() / 1_000_000;
    write!(f, "{}.{:03}", ms / 1_000, ms % 1_000)
}

impl core::fmt::Display for ClockTell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::Silent => Ok(()),
            Self::Venue { offset_ns, anchor_early_ns } => {
                write!(f, "wall=venue offset_ns={offset_ns} (the anchor law ran ")?;
                write_secs(f, anchor_early_ns)?;
                f.write_str(if anchor_early_ns >= 0 { " s early)" } else { " s late)" })
            }
            Self::Refused { offset_ns, before_epoch_ns } => {
                write!(f, "wall=anchor VENUE-FIT-REFUSED offset_ns={offset_ns} (it put the first record ")?;
                write_secs(f, before_epoch_ns)?;
                f.write_str(" s before the run's epoch: a stale-dominated fit)")
            }
        }
    }
}

impl ClockTell {
    /// The detail sidecar's per-run `wall` value: `venue` or `refused`,
    /// `None` on the anchor law (the key is then absent, so an anchor
    /// root's sidecar is byte-identical to before).
    #[must_use]
    pub const fn json_tag(&self) -> Option<&'static str> {
        match self {
            Self::Silent => None,
            Self::Venue { .. } => Some("venue"),
            Self::Refused { .. } => Some("refused"),
        }
    }
}

impl RunClock {
    /// The venue law when the run's HL ticks carried a stamp AND the fit
    /// is believable, else the old law — with the tell that says which.
    ///
    /// `fit` = `(offset, the first SAMPLED HL stamp)` from
    /// [`VenueOffsetFit::fit`]. The refusal is judged on that stamp —
    /// never on a surface's own first record — so `backtest` and
    /// `audit-pnl`, which pin different first records, cannot disagree
    /// about it.
    #[must_use]
    pub const fn choose(epoch_ns: u64, ts_first: u64, fit: Option<(i64, u64)>) -> (Self, ClockTell) {
        let anchor = Self::Anchor { epoch_ns, ts_first };
        let Some((offset_ns, fit_first_ts)) = fit else {
            return (anchor, ClockTell::Silent);
        };
        // i128: epoch and stamps are u64, the offset signed.
        let first_wall = fit_first_ts as i128 + offset_ns as i128;
        let before = epoch_ns as i128 - first_wall;
        if before > EPOCH_SKEW_TOLERANCE_NS as i128 {
            let before_epoch_ns = if before > i64::MAX as i128 { i64::MAX } else { before as i64 };
            return (anchor, ClockTell::Refused { offset_ns, before_epoch_ns });
        }
        let anchor_off = epoch_ns as i128 - ts_first as i128;
        let early = offset_ns as i128 - anchor_off;
        let anchor_early_ns = if early > i64::MAX as i128 {
            i64::MAX
        } else if early < i64::MIN as i128 {
            i64::MIN
        } else {
            early as i64
        };
        (Self::Venue { offset_ns }, ClockTell::Venue { offset_ns, anchor_early_ns })
    }

    /// The wall instant of a capture stamp.
    #[inline]
    #[must_use]
    pub const fn wall_of(&self, ts_ns: u64) -> u64 {
        match *self {
            Self::Anchor { epoch_ns, ts_first } => {
                epoch_ns.saturating_add(ts_ns.saturating_sub(ts_first))
            }
            Self::Venue { offset_ns } => ts_ns.saturating_add_signed(offset_ns),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty, VenueId};

    fn stamped(ts_ns: u64, venue_ms: u64) -> Tick {
        Tick::new_stamped(
            ts_ns,
            VenueId::Hyperliquid,
            0,
            0,
            Price::from_raw(1),
            Qty::from_raw(1),
            Price::from_raw(2),
            Qty::from_raw(1),
            venue_ms,
            0,
        )
    }

    #[test]
    fn the_offset_is_the_median_of_the_first_stamped_ticks() {
        let mut f = VenueOffsetFit::new();
        assert_eq!(f.offset_ns(), None, "no stamp, no opinion");
        // Unstamped ticks are ignored.
        f.observe(&stamped(5, 0));
        assert_eq!(f.offset_ns(), None);
        // Three stamps: offsets 10, 30, 20 ms ⇒ median 20 ms.
        f.observe(&stamped(1_000_000_000, 1_010));
        f.observe(&stamped(2_000_000_000, 2_030));
        f.observe(&stamped(3_000_000_000, 3_020));
        assert_eq!(f.offset_ns(), Some(20_000_000));
        // A fourth ⇒ even count, the mean of the two middles.
        f.observe(&stamped(4_000_000_000, 4_040));
        assert_eq!(f.offset_ns(), Some(25_000_000));
    }

    #[test]
    fn the_fit_takes_only_the_first_samples_and_shrugs_off_outliers() {
        let mut f = VenueOffsetFit::new();
        // Twenty stale snapshots a minute old at the start…
        let mut k = 0u64;
        while k < 20 {
            f.observe(&stamped(100_000_000_000 + k, 40_000));
            k += 1;
        }
        // …then fresh prints, 150 ms of feed delay.
        let mut k = 0u64;
        while k < 200 {
            let ts = 200_000_000_000 + k * 1_000_000;
            f.observe(&stamped(ts, (ts + 150_000_000) / 1_000_000));
            k += 1;
        }
        assert!(f.full());
        // 20 of 64 are outliers: the median is the fresh one.
        assert_eq!(f.offset_ns(), Some(150_000_000));
    }

    /// The SHARED clock fixture (`claude-worker/tests/fixtures/bin15/
    /// clock-1.*`): this law's offset is WRITTEN here under
    /// `BIN15_CLOCK_WRITE=1` and read by the worker's
    /// `tests/test_hip4.py`, which pins its Python mirror of this law to
    /// it exactly and `hip4.venue_wall_offset_ns`'s file-wide law to it
    /// within one second (plan 28 S2's parity bar).
    #[test]
    fn the_clock_fixture_offset_is_the_one_the_worker_reads() {
        const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../claude-worker/tests/fixtures/bin15/");
        let src = std::fs::read_to_string(format!("{DIR}clock-1.input.tsv")).expect("clock-1.input.tsv");
        let mut f = VenueOffsetFit::new();
        let mut rows = 0usize;
        for line in src.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let mut it = l.split('\t');
            let ts: u64 = it.next().expect("ts").parse().expect("ts");
            let vms: u64 = it.next().expect("vms").parse().expect("vms");
            f.observe(&stamped(ts, vms));
            rows += 1;
        }
        assert!(rows > 1_000, "the tape produced almost nothing");
        let got = f.offset_ns().expect("the tape carries stamps");
        let expected = format!("{DIR}clock-1.expected.tsv");
        if std::env::var("BIN15_CLOCK_WRITE").as_deref() == Ok("1") {
            let text = format!(
                "# clock-1.expected.tsv — WRITTEN by crates/cli/src/backtest/clock.rs \
                 (BIN15_CLOCK_WRITE=1).\n# the harness law: median of the first 64 stamped ticks\n\
                 first64_offset_ns\t{got}\n"
            );
            std::fs::write(&expected, text).expect("write expected");
            return;
        }
        let want = std::fs::read_to_string(&expected).expect("clock-1.expected.tsv (BIN15_CLOCK_WRITE=1)");
        let want: i64 = want
            .lines()
            .find_map(|l| l.strip_prefix("first64_offset_ns\t"))
            .expect("first64_offset_ns row")
            .trim()
            .parse()
            .expect("offset");
        assert_eq!(got, want, "the harness law moved — regenerate on purpose, never silently");
        // And it is the fixture's own truth to within the feed delay.
        assert!((got - 1_788_995_000_000_000_000).abs() < 400_000_000, "{got}");
    }

    /// The v2 law is kept bit for bit — including the clamp of a stamp
    /// before the anchor.
    #[test]
    fn a_run_without_a_venue_stamp_keeps_the_old_law() {
        let (c, tell) = RunClock::choose(1_000_000_000_000, 500, None);
        assert_eq!(c, RunClock::Anchor { epoch_ns: 1_000_000_000_000, ts_first: 500 });
        assert_eq!(c.wall_of(500), 1_000_000_000_000);
        assert_eq!(c.wall_of(1_500), 1_000_000_001_000);
        assert_eq!(c.wall_of(100), 1_000_000_000_000, "clamped at the anchor");
        assert_eq!(tell, ClockTell::Silent);
        assert_eq!(tell.to_string(), "", "the summary says nothing new");
    }

    /// A fit dominated by stale snapshots (more than half of the first
    /// 64 stamps a minute old) would put the run's first record BEFORE
    /// the directory was even named. That is refused, loudly, and the
    /// run keeps the anchor law.
    #[test]
    fn a_stale_dominated_fit_is_refused_not_believed() {
        const EPOCH: u64 = 1_789_192_800_000_000_000;
        const BOOT_MONO: u64 = 5_000_000_000_000;
        let true_off = (EPOCH - BOOT_MONO) as i64;
        let first = BOOT_MONO + 20_000_000_000; // 20 s after the dir
        let mut f = VenueOffsetFit::new();
        let mut k = 0u64;
        while k < 40 {
            // A minute stale: venue = true − 60 s.
            let ts = first + k * 1_000_000;
            let v = (ts as i64 + true_off - 60_000_000_000) as u64 / 1_000_000;
            f.observe(&stamped(ts, v));
            k += 1;
        }
        while k < 64 {
            let ts = first + k * 1_000_000;
            f.observe(&stamped(ts, (ts as i64 + true_off) as u64 / 1_000_000));
            k += 1;
        }
        let fit = f.fit();
        let (c, tell) = RunClock::choose(EPOCH, first, fit);
        assert_eq!(c, RunClock::Anchor { epoch_ns: EPOCH, ts_first: first }, "refused");
        assert!(matches!(tell, ClockTell::Refused { .. }), "{tell:?}");
        assert!(tell.to_string().starts_with("wall=anchor VENUE-FIT-REFUSED"), "{tell}");
        // Within the skew tolerance it is believed.
        let (c2, _) = RunClock::choose(EPOCH, first, Some((true_off - 21_000_000_000, first)));
        assert_eq!(c2, RunClock::Venue { offset_ns: true_off - 21_000_000_000 });
    }

    /// The case the law exists for: the directory is named at boot and
    /// the first tick lands 25 s later. On the old law every record is
    /// 25 s early; on the venue's clock it is where the venue put it.
    #[test]
    fn a_late_first_tick_no_longer_moves_the_whole_run_early() {
        const EPOCH: u64 = 1_789_192_800_000_000_000; // the dir's name
        const BOOT_MONO: u64 = 5_000_000_000_000; // mono stamp at the epoch
        const OFF: i64 = (EPOCH - BOOT_MONO) as i64; // venue − mono
        // The first tick arrives 25 s after the directory was named.
        let first = BOOT_MONO + 25_000_000_000;
        let (old, _) = RunClock::choose(EPOCH, first, None);
        let (new, tell) = RunClock::choose(EPOCH, first, Some((OFF, first)));
        // A roll received 9.5 s after a quarter-hour start S (venue).
        let s = EPOCH + 300_000_000_000;
        let roll_mono = s - EPOCH + BOOT_MONO + 9_500_000_000;
        assert_eq!(new.wall_of(roll_mono), s + 9_500_000_000, "+9.5 s: the receipt lag");
        let old_wall = old.wall_of(roll_mono) as i128;
        assert_eq!(old_wall - s as i128, -15_500_000_000, "−15.5 s on the old law");
        assert_eq!(
            tell.to_string(),
            format!("wall=venue offset_ns={OFF} (the anchor law ran 25.000 s early)")
        );
        // Sign-correct around zero: an anchor law 0.5 s LATE says so.
        let late = ClockTell::Venue { offset_ns: 1, anchor_early_ns: -500_000_000 };
        assert_eq!(late.to_string(), "wall=venue offset_ns=1 (the anchor law ran 0.500 s late)");
    }
}
