// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Bar sources (impl-06 §3.4, §4.1.1, §4.1.6): `SynBars` (SYN-1's flat chart), `SynTape` (the
//! realtime drive of `Driver::Rt`) and `ChartSource` (prepared bar files through `pine-screen`).
//!
//! Construction allocates (load time); `SynBars::bar`, `SynTape::{history_bar, tick, confirm,
//! htf_developing}` and `ChartSource::rows` on an already-open file write into caller-owned stack
//! `Bar`s or lend borrowed rows and never allocate — they are reached from the step loop.

use crate::fixture::RtSpec;
use crate::run::{RunError, SkipReason};
use crate::syn::SynSeq;
use pine_ir::{barflag, Bar};
use std::path::PathBuf;

/// SYN bars' constant price, ×1e6 (`1_000`).
pub const SYN_PRICE_1E6: i64 = 1_000 * 1_000_000;
/// SYN bars' constant volume field (`volume_1e6 = 1e6`).
pub const SYN_VOLUME_1E6: i64 = 1_000_000;

/// Bars for a `Syn` fixture: OHLC constant `1_000 × 1e6`, volume `1e6`, on the `tf_s` grid from
/// `from_ms`, `index` = step, `CONFIRMED` on every bar and `LAST | LAST_CONFIRMED_HISTORY` on the
/// last (impl-06 §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SynBars {
    n: u32,
    from_ms: i64,
    tf_ms: i64,
}

impl SynBars {
    /// `n` bars from `from_ms`, `tf_s` seconds each.
    pub fn new(n: u32, from_ms: i64, tf_s: u32) -> Self {
        debug_assert!(tf_s > 0);
        Self {
            n,
            from_ms,
            tf_ms: i64::from(tf_s) * 1000,
        }
    }

    /// Bar count.
    pub fn len(&self) -> u32 {
        self.n
    }

    /// No bar.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Writes bar `i` into `out` (a 64-byte POD by value — the designed-copy class).
    #[inline]
    pub fn bar(&self, i: u32, out: &mut Bar) {
        debug_assert!(i < self.n);
        let open_ms = self.from_ms + i64::from(i) * self.tf_ms;
        let mut flags = barflag::CONFIRMED;
        if i + 1 == self.n {
            flags |= barflag::LAST | barflag::LAST_CONFIRMED_HISTORY;
        }
        *out = Bar {
            open_1e6: SYN_PRICE_1E6,
            high_1e6: SYN_PRICE_1E6,
            low_1e6: SYN_PRICE_1E6,
            close_1e6: SYN_PRICE_1E6,
            volume_1e6: SYN_VOLUME_1E6,
            open_ms,
            close_ms: open_ms + self.tf_ms,
            index: i,
            flags,
        };
    }
}

/// `1000 + (s mod 100000)/100` at ×1e6, in exact integer arithmetic (`s` an integral MINSTD state).
#[inline]
fn syn_price_1e6(s: f64) -> i64 {
    debug_assert!((1.0..2_147_483_647.0).contains(&s) && s.fract() == 0.0);
    let s = s as i64;
    SYN_PRICE_1E6 + (s % 100_000) * 10_000
}

/// The realtime tape of `Driver::Rt` (§4.1.6): `history` confirmed bars, then `realtime` bars of
/// `ticks` forming updates each plus a confirm; tick prices from `SynSeq`, so every update moves.
///
/// `pin:06-syntape-one-sequence` — one `SynSeq` drives the whole tape: history bar `i` is flat at
/// step `i + 1`, tick `j` of realtime bar `r` is step `history + r·ticks + j + 1`.
/// `pin:06-syntape-volume-field` — "volume = (j + 1)·1e6" is the `volume_1e6` field (volume `j + 1`),
/// the reading `SynBars` uses for its "volume 1e6"; a history bar's volume field is `1e6`.
/// `pin:06-syntape-no-last` — no tape bar carries `LAST` (§4.1.6 names only
/// `LAST_CONFIRMED_HISTORY`, on the last history bar).
#[derive(Debug, Clone)]
pub struct SynTape {
    rt: RtSpec,
    tf_ms: i64,
    anchor_ms: i64,
    /// History bar prices ×1e6, `history` entries.
    hist_px: Vec<i64>,
    /// Per realtime tick, row-major `[r][j]`: `(price, running high, running low)` ×1e6.
    tick_px: Vec<[i64; 3]>,
}

impl SynTape {
    /// The tape of `rt` on the `tf_s` grid from `anchor_ms` (a multiple of `tf_s · htf_mult · 1000`,
    /// so `htf_mult` consecutive bars are the epoch-aligned HTF period). Load time; allocates.
    pub fn new(rt: RtSpec, tf_s: u32, anchor_ms: i64) -> Self {
        let tf_ms = i64::from(tf_s) * 1000;
        debug_assert!(tf_ms > 0 && rt.ticks > 0);
        debug_assert!(anchor_ms % (tf_ms * i64::from(rt.htf_mult.max(1))) == 0);
        let mut seq = SynSeq::new();
        let mut hist_px = Vec::with_capacity(rt.history as usize);
        let mut i = 0u32;
        while i < rt.history {
            hist_px.push(syn_price_1e6(seq.step().s));
            i += 1;
        }
        let ticks = rt.ticks as usize;
        let mut tick_px = Vec::with_capacity(rt.realtime as usize * ticks);
        let mut r = 0u32;
        while r < rt.realtime {
            let (mut hi, mut lo) = (i64::MIN, i64::MAX);
            let mut j = 0usize;
            while j < ticks {
                let px = syn_price_1e6(seq.step().s);
                hi = hi.max(px);
                lo = lo.min(px);
                tick_px.push([px, hi, lo]);
                j += 1;
            }
            r += 1;
        }
        Self {
            rt,
            tf_ms,
            anchor_ms,
            hist_px,
            tick_px,
        }
    }

    /// The spec the tape was built from.
    pub fn spec(&self) -> RtSpec {
        self.rt
    }

    /// Bar length, ms.
    pub fn tf_ms(&self) -> i64 {
        self.tf_ms
    }

    /// History bar `i` (`CONFIRMED`, plus `LAST_CONFIRMED_HISTORY` on the last).
    #[inline]
    pub fn history_bar(&self, i: u32, out: &mut Bar) {
        debug_assert!(i < self.rt.history);
        let px = self.hist_px[i as usize];
        let mut flags = barflag::CONFIRMED;
        if i + 1 == self.rt.history {
            flags |= barflag::LAST_CONFIRMED_HISTORY;
        }
        self.write(i, [px, px, px, px], SYN_VOLUME_1E6, flags, out);
    }

    /// Forming bar of realtime bar `r` after tick `j` (`REALTIME`, not `CONFIRMED`).
    #[inline]
    pub fn tick(&self, r: u32, j: u16, out: &mut Bar) {
        self.rt_bar(r, j, barflag::REALTIME, out);
    }

    /// Realtime bar `r` at its close (`REALTIME | CONFIRMED`).
    #[inline]
    pub fn confirm(&self, r: u32, out: &mut Bar) {
        self.rt_bar(
            r,
            self.rt.ticks - 1,
            barflag::REALTIME | barflag::CONFIRMED,
            out,
        );
    }

    /// The `htf_mult`-bar aggregates of the whole tape, confirmed (history of a sub-program).
    /// Load time; allocates.
    ///
    /// `pin:06-syntape-htf-complete-groups` — only groups whose period closes inside the tape are
    /// yielded (a trailing partial group never confirms); `index` is the group number (HTF rows are
    /// re-indexed from 0, §4.5.2). `pin:06-syntape-htf-flags` — `CONFIRMED`, plus `REALTIME` when the
    /// group's closing chart bar is a realtime bar (the chart bar's own flags at its confirm).
    pub fn htf_bars(&self) -> Vec<Bar> {
        let m = u32::from(self.rt.htf_mult);
        if m == 0 {
            return Vec::new();
        }
        let total = self.rt.history + self.rt.realtime;
        let groups = total / m;
        let mut v = Vec::with_capacity(groups as usize);
        let mut g = 0u32;
        while g < groups {
            let mut b = Bar::default();
            self.aggregate(g, (g + 1) * m, None, &mut b);
            b.flags = barflag::CONFIRMED;
            if (g + 1) * m > self.rt.history {
                b.flags |= barflag::REALTIME;
            }
            v.push(b);
            g += 1;
        }
        v
    }

    /// The developing HTF bar containing realtime bar `r` after tick `j` (`REALTIME`).
    #[inline]
    pub fn htf_developing(&self, r: u32, j: u16, out: &mut Bar) {
        debug_assert!(self.rt.htf_mult > 0);
        let h = self.rt.history + r;
        let g = h / u32::from(self.rt.htf_mult);
        self.aggregate(g, h, Some((r, j)), out);
        out.flags = barflag::REALTIME;
    }

    /// HTF bar `g` over chart bars `[g·m, end)` at their final values, plus `forming` (realtime bar,
    /// tick) when given. Flags are the caller's.
    fn aggregate(&self, g: u32, end: u32, forming: Option<(u32, u16)>, out: &mut Bar) {
        let m = u32::from(self.rt.htf_mult);
        let start = g * m;
        let htf_ms = self.tf_ms * i64::from(m);
        let open_ms = self.anchor_ms + i64::from(g) * htf_ms;
        let mut acc = [0i64, i64::MIN, i64::MAX, 0, 0]; // open, high, low, close, volume
        let mut first = true;
        let mut k = start;
        while k < end {
            let mut b = Bar::default();
            self.final_bar(k, &mut b);
            fold(&mut acc, &b, first);
            first = false;
            k += 1;
        }
        if let Some((r, j)) = forming {
            let mut b = Bar::default();
            self.tick(r, j, &mut b);
            fold(&mut acc, &b, first);
        }
        *out = Bar {
            open_1e6: acc[0],
            high_1e6: acc[1],
            low_1e6: acc[2],
            close_1e6: acc[3],
            volume_1e6: acc[4],
            open_ms,
            close_ms: open_ms + htf_ms,
            index: g,
            flags: 0,
        };
    }

    /// Chart bar `k` of the tape at its final values (a history bar, or a realtime bar's confirm).
    fn final_bar(&self, k: u32, out: &mut Bar) {
        if k < self.rt.history {
            self.history_bar(k, out);
        } else {
            self.confirm(k - self.rt.history, out);
        }
    }

    fn rt_bar(&self, r: u32, j: u16, flags: u32, out: &mut Bar) {
        debug_assert!(r < self.rt.realtime && j < self.rt.ticks);
        let base = r as usize * self.rt.ticks as usize;
        let o = self.tick_px[base][0];
        let [c, h, l] = self.tick_px[base + j as usize];
        let vol = (i64::from(j) + 1) * 1_000_000;
        self.write(self.rt.history + r, [o, h, l, c], vol, flags, out);
    }

    fn write(&self, index: u32, ohlc: [i64; 4], vol: i64, flags: u32, out: &mut Bar) {
        let open_ms = self.anchor_ms + i64::from(index) * self.tf_ms;
        *out = Bar {
            open_1e6: ohlc[0],
            high_1e6: ohlc[1],
            low_1e6: ohlc[2],
            close_1e6: ohlc[3],
            volume_1e6: vol,
            open_ms,
            close_ms: open_ms + self.tf_ms,
            index,
            flags,
        };
    }
}

#[inline]
fn fold(acc: &mut [i64; 5], b: &Bar, first: bool) {
    if first {
        acc[0] = b.open_1e6;
    }
    acc[1] = acc[1].max(b.high_1e6);
    acc[2] = acc[2].min(b.low_1e6);
    acc[3] = b.close_1e6;
    acc[4] = acc[4].saturating_add(b.volume_1e6);
}

/// One opened bar file and the key it was opened under.
struct Opened {
    symbol_key: String,
    tf_s: u32,
    clamp: pine_screen::Clamp,
    file: pine_screen::BarFile,
}

/// Prepared bar files through `pine-screen` (impl-07 v4 §3.3): the path from
/// `pine_screen::bar_path`, the rows from `pine_screen::BarFile::open_clamped` — validated,
/// re-indexed from 0, `GAP_BEFORE` cleared on row 0. One `BarFile` per `(symbol_key, tf_s, clamp)`,
/// kept for the life of the source.
pub struct ChartSource {
    root: Option<PathBuf>,
    open: Vec<Opened>,
}

impl ChartSource {
    /// A source over `root` (`None`: every request skips with `NoBarsRoot`).
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            open: Vec::new(),
        }
    }

    fn find(&self, symbol_key: &str, tf_s: u32, clamp: pine_screen::Clamp) -> Option<usize> {
        let mut i = 0usize;
        while i < self.open.len() {
            let o = &self.open[i];
            if o.tf_s == tf_s && o.clamp == clamp && o.symbol_key == symbol_key {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// `BarFile::rows()` of `bar_path(root, symbol_key, tf_s)` opened with `clamp`
    /// (`rows()[i].index == i`). No root → `Skip(NoBarsRoot)`; no such file (`NotFound` on the
    /// path) → `Skip(NoBarFile)`; any `BarFileErr` (`Empty` included) → `RunError::BarFile` (7001).
    /// Load time on the first request for a key (allocates); a lookup afterwards.
    pub fn rows(
        &mut self,
        symbol_key: &str,
        tf_s: u32,
        clamp: pine_screen::Clamp,
    ) -> Result<&[Bar], RunError> {
        if let Some(i) = self.find(symbol_key, tf_s, clamp) {
            return Ok(self.open[i].file.rows());
        }
        let root = match &self.root {
            Some(r) => r,
            None => return Err(RunError::Skip(SkipReason::NoBarsRoot)),
        };
        let path =
            pine_screen::bar_path(root, symbol_key, tf_s).map_err(|err| RunError::BarFile {
                path: root.clone(),
                err,
            })?;
        if let Err(e) = std::fs::metadata(&path) {
            if e.kind() == std::io::ErrorKind::NotFound {
                // COPY: the symbol key (≤ 64 B) once per missing file, on the skip path (load
                // time) — the skip names the symbol as well as the timeframe (r33j, D047) —
                // a `&'static str` lookup over the ticker map rejected (any key may be asked for).
                return Err(RunError::Skip(SkipReason::NoBarFile {
                    symbol_key: symbol_key.to_owned(),
                    tf_s,
                }));
            }
        }
        let file = match pine_screen::BarFile::open_clamped(&path, clamp) {
            Ok(f) => f,
            Err(err) => return Err(RunError::BarFile { path, err }),
        };
        // COPY: the symbol key (≤ 64 B) once per opened file, at load — the cache outlives the
        // caller's `&str` — borrowing the fixture's key rejected (the source spans every fixture).
        self.open.push(Opened {
            symbol_key: symbol_key.to_owned(),
            tf_s,
            clamp,
            file,
        });
        let last = self.open.len() - 1;
        Ok(self.open[last].file.rows())
    }

    /// `BarFile::hdr()` of a file already opened (`src_digest64`, `holes` for the report).
    pub fn hdr(
        &self,
        symbol_key: &str,
        tf_s: u32,
        clamp: pine_screen::Clamp,
    ) -> Option<&pine_screen::BarFileHdr> {
        self.find(symbol_key, tf_s, clamp)
            .map(|i| self.open[i].file.hdr())
    }

    /// `BarFile::rows()` of a file already opened by [`ChartSource::rows`] — the shared borrow the
    /// runner takes once every key of a run is open (the chart and each sub-program at once).
    pub fn opened(&self, symbol_key: &str, tf_s: u32, clamp: pine_screen::Clamp) -> Option<&[Bar]> {
        self.find(symbol_key, tf_s, clamp)
            .map(|i| self.open[i].file.rows())
    }
}
