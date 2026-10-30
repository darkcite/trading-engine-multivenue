// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The row record, the row files and the one accumulator (§3.6, §4.5). The rows files are
//! offline append/read paths (one 120-byte `write_all` per row); the accumulator is the bar loop's.

use pine_ir::{trial_flag, Bar, SizingMode, TradeRec, TrialSummary};

/// One appended record: the trial id and its frozen `TrialSummary`, raw (contract §3.16.1). The
/// little-endian codec is the cold `codec` module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct RowRec {
    /// The trial (tier 1: `ids::trial_id`; tier 2: the hand-off's `t2_trial_id`; impl-10: its own).
    pub trial_id: [u8; 32],
    /// Contract §3.16.1, 88 bytes.
    pub summary: TrialSummary,
}
const _: () = assert!(core::mem::size_of::<RowRec>() == 120);
const _: () = assert!(core::mem::align_of::<RowRec>() == 8);
const _: () = assert!(core::mem::offset_of!(RowRec, summary) == 32);

/// `b"PVMROWS1"`.
pub const ROWS_MAGIC: [u8; 8] = *b"PVMROWS1";
/// Format version.
pub const ROWS_VERSION: u32 = 1;
/// `size_of::<RowRec>()`.
const REC_SIZE: u32 = 120;

/// The 64-byte header of a rows file. One file holds one tier, one notional, one fee set (row 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct RowsHdr {
    /// [`ROWS_MAGIC`].
    pub magic: [u8; 8],
    /// [`ROWS_VERSION`].
    pub version: u32,
    /// `size_of::<RowRec>()` = 120.
    pub rec_size: u32,
    /// 1 = the tier-1 screen (and its shadow runs), 2 = tier-2 replay (B'3).
    pub tier: u8,
    /// Always zero.
    pub _pad0: [u8; 7],
    /// The file's one selection notional, USD ×1e6.
    pub sizing_notional_1e6: i64,
    /// `ids::fee_digest` of the `--fee-bps` set every row was charged under.
    pub fee_digest: [u8; 32],
}
const _: () = assert!(core::mem::size_of::<RowsHdr>() == 64);
const _: () = assert!(core::mem::align_of::<RowsHdr>() == 8);

impl RowsHdr {
    /// A tier-`tier` header for `notional_1e6` and `fee_digest` (magic, version, size filled).
    pub fn new(tier: u8, sizing_notional_1e6: i64, fee_digest: [u8; 32]) -> RowsHdr {
        RowsHdr {
            magic: ROWS_MAGIC,
            version: ROWS_VERSION,
            rec_size: REC_SIZE,
            tier,
            _pad0: [0; 7],
            sizing_notional_1e6,
            fee_digest,
        }
    }

    /// Magic, version, record size and `_pad0` as the format fixes them.
    // pin:07-rows-pad0: a non-zero `_pad0` ("always zero") is `BadHeader`.
    fn well_formed(&self) -> bool {
        self.magic == ROWS_MAGIC
            && self.version == ROWS_VERSION
            && self.rec_size == REC_SIZE
            && self._pad0 == [0; 7]
    }

    /// Same tier, notional and fee set.
    fn same_file(&self, o: &RowsHdr) -> bool {
        self.tier == o.tier
            && self.sizing_notional_1e6 == o.sizing_notional_1e6
            && self.fee_digest == o.fee_digest
    }
}

/// Why a rows file operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowsErr {
    /// I/O (`raw_os_error` or −1).
    Io(i32),
    /// Magic, version or record size wrong.
    BadHeader,
    /// The existing file's tier, notional or fee digest differs from the writer's.
    HeaderMismatch,
    /// `(len − 64) % 120 ≠ 0` — a torn tail (a crash mid-append); never silently truncated.
    TornTail {
        /// File length.
        len: u64,
    },
    /// A row without `FIXED_NOTIONAL`, with another notional than the file's, or `_pad ≠ 0` (I18).
    RowRefused,
}

fn rows_io(e: &std::io::Error) -> RowsErr {
    RowsErr::Io(e.raw_os_error().unwrap_or(-1))
}

/// Read and check the header of an existing rows file of length `len`.
// pin:07-rows-short-header: a non-empty file shorter than its 64-byte header is `BadHeader`.
fn read_rows_hdr(f: &mut std::fs::File, len: u64) -> Result<RowsHdr, RowsErr> {
    use std::io::Read;
    if len < 64 {
        return Err(RowsErr::BadHeader);
    }
    let mut hb = [0u8; 64];
    f.read_exact(&mut hb).map_err(|e| rows_io(&e))?;
    let h = RowsHdr::from_le_bytes(&hb);
    if !h.well_formed() {
        return Err(RowsErr::BadHeader);
    }
    if (len - 64) % u64::from(REC_SIZE) != 0 {
        return Err(RowsErr::TornTail { len });
    }
    Ok(h)
}

/// Append-only writer (`O_APPEND`, one `write_all` of 120 bytes per row).
pub struct RowsWriter {
    file: std::fs::File,
    hdr: RowsHdr,
}

impl RowsWriter {
    /// Create the file with `hdr`, or open it and require the same header.
    // pin:07-rows-empty-file: an existing 0-byte file (a crash between create and the header
    // write) is created afresh.
    pub fn open(path: &std::path::Path, hdr: &RowsHdr) -> Result<RowsWriter, RowsErr> {
        use std::io::Write;
        if !hdr.well_formed() {
            return Err(RowsErr::BadHeader);
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .map_err(|e| rows_io(&e))?;
        let len = file.metadata().map_err(|e| rows_io(&e))?.len();
        if len == 0 {
            file.write_all(&hdr.to_le_bytes())
                .map_err(|e| rows_io(&e))?;
        } else {
            let have = read_rows_hdr(&mut file, len)?;
            if !have.same_file(hdr) {
                return Err(RowsErr::HeaderMismatch);
            }
        }
        Ok(RowsWriter { file, hdr: *hdr })
    }

    /// Append one row (I18 checked first).
    pub fn append(&mut self, r: &RowRec) -> Result<(), RowsErr> {
        use std::io::Write;
        let s = &r.summary;
        if s.flags & trial_flag::FIXED_NOTIONAL == 0
            || s.sizing_notional_1e6 != self.hdr.sizing_notional_1e6
            || s._pad != 0
        {
            return Err(RowsErr::RowRefused);
        }
        self.file
            .write_all(&r.to_le_bytes())
            .map_err(|e| rows_io(&e))
    }

    /// `fsync` and close.
    pub fn finish(self) -> Result<(), RowsErr> {
        self.file.sync_all().map_err(|e| rows_io(&e))
    }
}

/// A whole rows file read into memory (offline).
pub struct RowsFile {
    hdr: RowsHdr,
    recs: Vec<RowRec>,
}

impl RowsFile {
    /// Read and validate.
    pub fn open(path: &std::path::Path) -> Result<RowsFile, RowsErr> {
        use std::io::Read;
        let mut f = std::fs::File::open(path).map_err(|e| rows_io(&e))?;
        let len = f.metadata().map_err(|e| rows_io(&e))?.len();
        let hdr = read_rows_hdr(&mut f, len)?;
        let n = ((len - 64) / u64::from(REC_SIZE)) as usize;
        let mut recs = Vec::with_capacity(n);
        let mut b = [0u8; 120];
        let mut i = 0;
        while i < n {
            f.read_exact(&mut b).map_err(|e| rows_io(&e))?;
            recs.push(RowRec::from_le_bytes(&b));
            i += 1;
        }
        Ok(RowsFile { hdr, recs })
    }

    /// The header.
    pub fn hdr(&self) -> &RowsHdr {
        &self.hdr
    }

    /// The rows, in append order.
    pub fn recs(&self) -> &[RowRec] {
        &self.recs
    }

    /// The row of `trial_id` (the last one when appended twice).
    pub fn find(&self, trial_id: &[u8; 32]) -> Option<&RowRec> {
        self.recs.iter().rev().find(|r| &r.trial_id == trial_id)
    }
}

/// How a `TrialAccum` is configured for one trial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccumCfg {
    /// `FixedNotional { notional_1e6 }` for every selection row.
    pub sizing: SizingMode,
    /// `ChartInfo::pointvalue_1e6`.
    pub pointvalue_1e6: i64,
    /// Scoring window start (UTC day boundary).
    pub from_ms: i64,
    /// Scoring window end, exclusive (UTC day boundary).
    pub to_ms: i64,
    /// Set `INTRABAR_APPROX` (tier 1: `meta.uses_intrabar`; tier 2: false).
    pub intrabar_approx: bool,
}

/// How a trial ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialEnd {
    /// Ran to the end of its bars.
    Completed,
    /// Stopped on a script fault.
    Faulted,
}

const DAY_MS: i64 = 86_400_000;
/// `(money ×1e12) × 100 / (N ×1e6)` = bps ×1e4 of N (§4.5.1).
const BPS_MUL: i128 = 100;
/// Clamp of `net` and `fee` before the gross identity (§4.5.1).
const HALF: i128 = (i64::MAX / 2) as i128;

/// **The** `TrialSummary` builder (§4.5): tier 1 feeds it from `TvBroker`, B'3 from
/// `NativeBroker` — one definition of every field, so a delta compares like with like.
/// Allocation only in `reserve`.
pub struct TrialAccum {
    cfg: Option<AccumCfg>,
    notional_1e6: i64,
    bars: u32,
    trades: u32,
    closed: u32,
    e0: i128,
    c0: i128,
    based: bool,
    /// Last stepped bar index (open trades at the end count through it).
    last_index: u32,
    /// First / last in-window bar index (`u32::MAX` / 0 until one is seen).
    win_lo: u32,
    win_hi: u32,
    /// `hold[h]` = closed trades held `h` bars.
    hold: Vec<u32>,
    /// Entry notional ×1e18 USD, difference array over bar indices (`len = max_bars + 1`).
    conc: Vec<i128>,
    /// Equity after the last in-window bar of each window day; `has_eq[d]` whether one was seen.
    eq_day: Vec<i128>,
    has_eq: Vec<bool>,
    /// Σ fill notional ×1e18 USD per window day.
    turn: Vec<i128>,
    /// Scratch for the p99 (sorted at `finish`).
    turn_sorted: Vec<i128>,
    /// The daily return series of the last `finish`.
    ret: Vec<f64>,
    n_days: usize,
}

impl Default for TrialAccum {
    fn default() -> TrialAccum {
        TrialAccum::new()
    }
}

impl TrialAccum {
    /// Empty (no allocation).
    pub fn new() -> TrialAccum {
        TrialAccum {
            cfg: None,
            notional_1e6: 0,
            bars: 0,
            trades: 0,
            closed: 0,
            e0: 0,
            c0: 0,
            based: false,
            last_index: 0,
            win_lo: u32::MAX,
            win_hi: 0,
            hold: Vec::new(),
            conc: Vec::new(),
            eq_day: Vec::new(),
            has_eq: Vec::new(),
            turn: Vec::new(),
            turn_sorted: Vec::new(),
            ret: Vec::new(),
            n_days: 0,
        }
    }

    /// Load time: capacity for `max_bars` stepped bars and `max_days` window days. Never shrinks.
    pub fn reserve(&mut self, max_bars: u32, max_days: u32) {
        let b = max_bars as usize + 2;
        let d = max_days as usize;
        if self.hold.len() < b {
            self.hold.resize(b, 0);
            self.conc.resize(b, 0);
        }
        if self.eq_day.len() < d {
            self.eq_day.resize(d, 0);
            self.has_eq.resize(d, false);
            self.turn.resize(d, 0);
            self.turn_sorted.resize(d, 0);
            self.ret.resize(d, 0.0);
        }
    }

    /// Start a trial. Panics (load-time defect) if `reserve` was too small.
    pub fn begin(&mut self, cfg: &AccumCfg) {
        let notional = match cfg.sizing {
            SizingMode::FixedNotional { notional_1e6 } => notional_1e6,
            SizingMode::AsDeclared => 0,
        };
        assert!(
            notional > 0,
            "TrialAccum: a row is sized FixedNotional (I18)"
        );
        let days = (cfg.to_ms - cfg.from_ms).div_euclid(DAY_MS);
        assert!(
            days >= 0 && days as usize <= self.eq_day.len(),
            "TrialAccum::reserve too small"
        );
        self.cfg = Some(*cfg);
        self.notional_1e6 = notional;
        self.n_days = days as usize;
        self.bars = 0;
        self.trades = 0;
        self.closed = 0;
        self.e0 = 0;
        self.c0 = 0;
        self.based = false;
        self.last_index = 0;
        self.win_lo = u32::MAX;
        self.win_hi = 0;
        self.hold.fill(0);
        self.conc.fill(0);
        self.eq_day[..self.n_days].fill(0);
        self.has_eq[..self.n_days].fill(false);
        self.turn[..self.n_days].fill(0);
        self.ret[..self.n_days].fill(0.0);
    }

    /// Once, before the first scored bar: equity and cumulative commission at the window start.
    pub fn baseline(&mut self, equity_1e12: i128, commission_paid_1e12: i128) {
        self.e0 = equity_1e12;
        self.c0 = commission_paid_1e12;
        self.based = true;
    }

    fn day_of(&self, t_ms: i64) -> Option<usize> {
        let cfg = self.cfg.as_ref()?;
        if t_ms < cfg.from_ms || t_ms >= cfg.to_ms {
            return None;
        }
        Some((t_ms - cfg.from_ms).div_euclid(DAY_MS) as usize)
    }

    /// `|qty| × px × pointvalue`, USD ×1e18.
    fn notional_1e18(&self, qty_1e6: i64, px_1e6: i64) -> i128 {
        let pv = self.cfg.map_or(0, |c| c.pointvalue_1e6);
        i128::from(qty_1e6)
            .saturating_abs()
            .saturating_mul(i128::from(px_1e6))
            .saturating_mul(i128::from(pv))
    }

    /// After the bar's last evaluation: its mark-to-market equity (`BrokerView::equity_1e12`).
    #[inline]
    pub fn on_bar(&mut self, bar: &Bar, equity_1e12: i128) {
        self.bars += 1;
        self.last_index = bar.index;
        let (from, to) = match self.cfg {
            Some(c) => (c.from_ms, c.to_ms),
            None => return,
        };
        if bar.open_ms >= from && bar.close_ms <= to {
            if self.win_lo == u32::MAX {
                self.win_lo = bar.index;
            }
            self.win_hi = bar.index;
            // The day a bar belongs to: the one its last millisecond is in (§4.5.1).
            let d = (bar.close_ms - 1 - from).div_euclid(DAY_MS) as usize;
            if d < self.n_days {
                self.eq_day[d] = equity_1e12;
                self.has_eq[d] = true;
            }
        }
    }

    fn add_conc(&mut self, from_bar: u32, to_bar: u32, v: i128) {
        let (a, b) = (from_bar as usize, to_bar as usize + 1);
        if a < self.conc.len() {
            self.conc[a] = self.conc[a].saturating_add(v);
        }
        if b < self.conc.len() {
            self.conc[b] = self.conc[b].saturating_sub(v);
        }
    }

    fn add_turn(&mut self, t_ms: i64, v: i128) {
        // pin:07-turnover-day-of-fill: a fill's day is its time's UTC day (`t.div_euclid(1 d)`),
        // the trade's `entry_ms` / `exit_ms` (the fill bar's time in TV mode).
        if let Some(d) = self.day_of(t_ms) {
            if d < self.n_days {
                self.turn[d] = self.turn[d].saturating_add(v);
            }
        }
    }

    /// Each closed trade, once, as it closes.
    #[inline]
    pub fn on_closed_trade(&mut self, t: &TradeRec) {
        let from = self.cfg.map_or(i64::MIN, |c| c.from_ms);
        if t.exit_ms < from {
            return; // closed in warm-up: inside the baseline (§4.5.1)
        }
        self.closed += 1;
        self.trades += 1;
        let h = t.exit_bar.saturating_sub(t.entry_bar) as usize;
        let last = self.hold.len() - 1;
        self.hold[h.min(last)] += 1;
        // pin:07-concurrency-inclusive: a trade counts as open on every bar from its entry bar
        // through its exit bar inclusive ("a trade opened and closed in one bar counts on that bar").
        let ne = self.notional_1e18(t.qty_1e6, t.entry_px_1e6);
        self.add_conc(t.entry_bar, t.exit_bar, ne);
        self.add_turn(t.entry_ms, ne);
        let nx = self.notional_1e18(t.qty_1e6, t.exit_px_1e6);
        self.add_turn(t.exit_ms, nx);
    }

    /// Each trade still open at the end, once.
    pub fn on_open_trade_at_end(&mut self, t: &TradeRec) {
        self.trades += 1;
        let ne = self.notional_1e18(t.qty_1e6, t.entry_px_1e6);
        self.add_conc(t.entry_bar, self.last_index, ne);
        self.add_turn(t.entry_ms, ne);
    }

    fn bps(&self, v: i128, ceil: bool) -> i128 {
        let num = v.saturating_mul(BPS_MUL);
        let n = i128::from(self.notional_1e6);
        let q = num.div_euclid(n);
        if ceil && num.rem_euclid(n) != 0 {
            q + 1
        } else {
            q
        }
    }

    /// The row. `commission_paid_1e12` = `RunRecord::commission_paid_1e12` at the end;
    /// `max_insns` = the max `VmCounters::max_insns` over the trial's instances; `extra_flags` =
    /// the host's (B'3: `UNJUDGED`; tier 1: 0).
    pub fn finish(
        &mut self,
        commission_paid_1e12: i128,
        max_insns: u32,
        end: TrialEnd,
        extra_flags: u32,
    ) -> TrialSummary {
        let cfg = self.cfg.expect("TrialAccum::begin first");
        let mut flags = trial_flag::FIXED_NOTIONAL | extra_flags;
        if cfg.intrabar_approx {
            flags |= trial_flag::INTRABAR_APPROX;
        }
        // Hold statistics: nearest rank over the histogram.
        let rank = |num: u64| -> u32 {
            let n = u64::from(self.closed);
            if n == 0 {
                return 0;
            }
            let r = (num * n).div_ceil(100).max(1);
            let mut acc = 0u64;
            let mut h = 0;
            while h < self.hold.len() {
                acc += u64::from(self.hold[h]);
                if acc >= r {
                    return h as u32;
                }
                h += 1;
            }
            0
        };
        let mut s = TrialSummary {
            bars: self.bars,
            trades: self.trades,
            closed: self.closed,
            obs_days: 0,
            hold_bars_med: rank(50),
            hold_bars_p95: rank(95),
            insn_p9999: max_insns,
            faults: 0,
            sizing_notional_1e6: self.notional_1e6,
            flags,
            ..TrialSummary::default()
        };
        if end == TrialEnd::Faulted {
            // I13: no numbers — P&L, obs_days, concurrency and turnover stay 0; no return series.
            s.faults = 1;
            s.flags |= trial_flag::FAULTED;
            self.n_days = 0;
            return s;
        }
        debug_assert!(self.based, "TrialAccum: baseline before finish");
        // Daily series from the row's own floored integers (§4.5.2).
        let mut e = self.e0;
        let mut prev_n: i128 = 0;
        let mut d = 0;
        while d < self.n_days {
            if self.has_eq[d] {
                e = self.eq_day[d];
            }
            let n = self.bps(e.saturating_sub(self.e0), false);
            self.ret[d] = (n - prev_n) as f64 / 10_000.0;
            prev_n = n;
            d += 1;
        }
        let net = prev_n.clamp(-HALF, HALF);
        let fee = self
            .bps(commission_paid_1e12.saturating_sub(self.c0), true)
            .clamp(-HALF, HALF);
        s.obs_days = self.n_days as u32;
        s.net_bps_1e4 = net as i64;
        s.fee_bps_1e4 = fee as i64;
        s.gross_bps_1e4 = (net + fee) as i64;
        // Max concurrent notional over window bars.
        let mut run: i128 = 0;
        let mut best: i128 = 0;
        let mut b = 0usize;
        while b < self.conc.len() {
            run = run.saturating_add(self.conc[b]);
            if (b as u32) >= self.win_lo && (b as u32) <= self.win_hi && run > best {
                best = run;
            }
            b += 1;
        }
        s.max_concurrent_1e6 = ceil_1e12(best);
        // p99 daily turnover, nearest rank over every window day.
        if self.n_days > 0 {
            let t = &mut self.turn_sorted[..self.n_days];
            // COPY: the window's day turnovers, n_days × 16 B, once per trial at finish — the
            // nearest-rank p99 needs a sorted scratch while `turn` keeps the day order — sorting
            // `turn` in place rejected: it would scramble the per-day sums.
            t.copy_from_slice(&self.turn[..self.n_days]);
            t.sort_unstable();
            let r = (99 * self.n_days).div_ceil(100).max(1);
            s.turnover_day_p99_1e6 = ceil_1e12(t[r - 1]);
        }
        s
    }

    /// One value per window day: that day's net P&L in bps of the notional, `0.0` on a flat day
    /// (contract §3.16.2), built from the row's own floored integers so that it sums to
    /// `net_bps_1e4` (§4.5.2).
    pub fn day_returns(&self) -> &[f64] {
        &self.ret[..self.n_days]
    }
}

/// USD ×1e18 → USD ×1e6, ceiling, clamped to `i64`.
fn ceil_1e12(v: i128) -> i64 {
    const S: i128 = 1_000_000_000_000;
    let q = v.div_euclid(S) + i128::from(v.rem_euclid(S) != 0);
    q.clamp(0, i128::from(i64::MAX)) as i64
}
