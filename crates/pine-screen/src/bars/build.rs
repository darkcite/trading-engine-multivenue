// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! COPY-DOCTRINE: the offline preparer — builds and publishes bar files once per dataset; no screen reaches it.

use super::{row_bytes, validate_rows, BarFileErr, BarFileHdr};
use core_time::barclock;
use pine_ir::{barflag, Bar};
use std::io::Write;
use std::path::Path;

const WEEK_S: u32 = 604_800;
const DAY_S: u32 = 86_400;

/// Whether bars of `src_tf_s` tile bars of `tf_s` on the barclock grid: `tf % src == 0` for sub-week
/// targets; `src ≤ 86400 && 86400 % src == 0` for the week and the month.
pub const fn tiles(src_tf_s: u32, tf_s: u32) -> bool {
    if src_tf_s == 0 {
        return false;
    }
    if tf_s < WEEK_S {
        tf_s % src_tf_s == 0
    } else {
        src_tf_s <= DAY_S && DAY_S % src_tf_s == 0
    }
}

/// Decimal text → ×1e6, **round half away from zero** (contract §3.16.5's rounding). Accepts
/// `[+-]?digits[.digits][(e|E)[+-]?digits]` — any digit count (the shell's REAL text may carry an exponent);
/// `None` for anything else or out of `i64` range. The candles scale point (§4.3.5).
// pin:07-dec-grammar: literal grammar — at least one integer digit, and a `.` must be followed by
// at least one digit (".5", "5." and "1e" are `None`).
pub fn dec_to_1e6(s: &[u8]) -> Option<i64> {
    let mut i = 0usize;
    let neg = match s.first() {
        Some(b'-') => {
            i = 1;
            true
        }
        Some(b'+') => {
            i = 1;
            false
        }
        _ => false,
    };
    let int_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let int_end = i;
    if int_end == int_start {
        return None;
    }
    let (mut frac_start, mut frac_end) = (i, i);
    if i < s.len() && s[i] == b'.' {
        i += 1;
        frac_start = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        frac_end = i;
        if frac_end == frac_start {
            return None;
        }
    }
    let mut exp: i64 = 0;
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        i += 1;
        let eneg = match s.get(i) {
            Some(b'-') => {
                i += 1;
                true
            }
            Some(b'+') => {
                i += 1;
                false
            }
            _ => false,
        };
        let e0 = i;
        while i < s.len() && s[i].is_ascii_digit() {
            // Saturate far beyond any meaningful shift; the digit count is unbounded.
            exp = (exp * 10 + i64::from(s[i] - b'0')).min(1 << 40);
            i += 1;
        }
        if i == e0 {
            return None;
        }
        if eneg {
            exp = -exp;
        }
    }
    if i != s.len() {
        return None;
    }
    // The digit string d = int ‖ frac; value = 0.d × 10^ni × 10^exp. `k` digits of d lie in the
    // integer part of value × 1e6; d[k] (if any) is the rounding digit.
    let ni = (int_end - int_start) as i64;
    let nf = (frac_end - frac_start) as i64;
    let total = ni + nf;
    let k = ni.saturating_add(exp).saturating_add(6);
    let digit = |j: i64| -> u8 {
        let j = j as usize;
        let ii = int_end - int_start;
        if j < ii {
            s[int_start + j] - b'0'
        } else {
            s[frac_start + (j - ii)] - b'0'
        }
    };
    const LIM: u128 = 1u128 << 63; // |i64::MIN|
    let mut acc: u128 = 0;
    let mut j = 0i64;
    let kept = k.clamp(0, total);
    while j < kept {
        acc = acc * 10 + u128::from(digit(j));
        if acc > LIM {
            return None;
        }
        j += 1;
    }
    if k > total && acc != 0 {
        let mut z = k - total;
        while z > 0 {
            acc *= 10;
            if acc > LIM {
                return None;
            }
            z -= 1;
        }
    }
    if k >= 0 && k < total && digit(k) >= 5 {
        acc += 1;
    }
    if neg {
        if acc > LIM {
            return None;
        }
        Some((acc as i128).wrapping_neg() as i64)
    } else if acc >= LIM {
        None
    } else {
        Some(acc as i64)
    }
}

/// One venue candle, already scaled by the host (`dec_to_1e6`, `open_ts` → ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct CandleRow {
    /// Bar open, UTC ms, on the source timeframe's grid.
    pub open_ms: i64,
    /// Open ×1e6.
    pub open_1e6: i64,
    /// High ×1e6.
    pub high_1e6: i64,
    /// Low ×1e6.
    pub low_1e6: i64,
    /// Close ×1e6.
    pub close_1e6: i64,
    /// Volume ×1e6, the venue's native unit.
    pub volume_1e6: i64,
}
const _: () = assert!(core::mem::size_of::<CandleRow>() == 48);
const _: () = assert!(core::mem::align_of::<CandleRow>() == 8);

/// One captured trade print (contract §3.15.2), its bucketing time resolved by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct TradePrint {
    /// `venue_time_ms` when non-zero, else `scale::ms_from_ns` of the rebased wall time.
    pub t_ms: i64,
    /// `ChannelEvent::v0`, price ×1e6.
    pub px_1e6: i64,
    /// `ChannelEvent::v1`, signed quantity ×1e6 (Deribit: USD notional) — volume adds `|v1|`.
    pub qty_1e6: i64,
    /// `ChannelEvent::venue_seq`, the tie-break after `t_ms`.
    pub seq: u64,
}
const _: () = assert!(core::mem::size_of::<TradePrint>() == 32);
const _: () = assert!(core::mem::align_of::<TradePrint>() == 8);

/// What a builder produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BuildStats {
    /// Rows written.
    pub rows: u64,
    /// Holes between them.
    pub holes: u32,
    /// Buckets dropped as incomplete (aggregation) or outside coverage (trades).
    pub dropped_partial: u32,
    /// Rows whose volume saturated (`VOL_SAT`).
    pub vol_saturated: u32,
}

/// Append one folded bucket to `out` with its index and flags (`GAP_BEFORE` after a hole,
/// `VOL_SAT` exactly when the volume is `i64::MAX`); updates `st`.
fn push_row(out: &mut Vec<Bar>, mut b: Bar, tf_s: u32, st: &mut BuildStats) {
    b.index = out.len() as u32;
    b.close_ms = barclock::close_ms(b.open_ms, tf_s);
    b.flags = barflag::CONFIRMED;
    if let Some(p) = out.last() {
        let gap = barclock::bars_between(p.open_ms, b.open_ms, tf_s) - 1;
        if gap > 0 {
            b.flags |= barflag::GAP_BEFORE;
            st.holes = st
                .holes
                .saturating_add(u32::try_from(gap).unwrap_or(u32::MAX));
        }
    }
    if b.volume_1e6 == i64::MAX {
        b.flags |= barflag::VOL_SAT;
        st.vol_saturated += 1;
    }
    out.push(b);
}

/// Candles of `src_tf_s` (ascending) → rows of `tf_s`: equal timeframes copy, a tiling source
/// aggregates **complete buckets only** (§4.2.3). `out` is cleared first.
pub fn bars_from_candles(
    src: &[CandleRow],
    src_tf_s: u32,
    tf_s: u32,
    out: &mut Vec<Bar>,
) -> Result<BuildStats, BarFileErr> {
    out.clear();
    if src_tf_s != tf_s && !tiles(src_tf_s, tf_s) {
        return Err(BarFileErr::NotADivisor { src_tf_s, tf_s });
    }
    if !barclock::is_allowed(src_tf_s) {
        return Err(BarFileErr::BadTimeframe(src_tf_s));
    }
    if !barclock::is_allowed(tf_s) {
        return Err(BarFileErr::BadTimeframe(tf_s));
    }
    let mut st = BuildStats::default();
    // The bucket being folded: (open of the target bucket, the fold, source bars seen).
    let mut cur: Option<(i64, Bar, i64)> = None;
    let mut i = 0;
    while i < src.len() {
        let c = &src[i];
        let row = i as u64;
        if barclock::open_ms(c.open_ms, src_tf_s) != c.open_ms {
            return Err(BarFileErr::BoundaryMismatch { row });
        }
        if i > 0 && c.open_ms <= src[i - 1].open_ms {
            return Err(BarFileErr::NotAscending { row });
        }
        let bo = barclock::open_ms(c.open_ms, tf_s);
        match &mut cur {
            Some((o, f, cnt)) if *o == bo => {
                f.high_1e6 = f.high_1e6.max(c.high_1e6);
                f.low_1e6 = f.low_1e6.min(c.low_1e6);
                f.close_1e6 = c.close_1e6;
                f.volume_1e6 = f.volume_1e6.saturating_add(c.volume_1e6);
                *cnt += 1;
            }
            _ => {
                if let Some((o, f, cnt)) = cur.take() {
                    flush(out, o, f, cnt, src_tf_s, tf_s, &mut st);
                }
                let f = Bar {
                    open_1e6: c.open_1e6,
                    high_1e6: c.high_1e6,
                    low_1e6: c.low_1e6,
                    close_1e6: c.close_1e6,
                    volume_1e6: c.volume_1e6,
                    open_ms: bo,
                    ..Bar::default()
                };
                cur = Some((bo, f, 1));
            }
        }
        i += 1;
    }
    if let Some((o, f, cnt)) = cur.take() {
        flush(out, o, f, cnt, src_tf_s, tf_s, &mut st);
    }
    if out.is_empty() {
        return Err(BarFileErr::Empty);
    }
    st.holes = validate_rows(out, tf_s)?;
    st.rows = out.len() as u64;
    Ok(st)
}

/// Emit a folded bucket iff every source bar of it was present (§4.2.3); else count it dropped.
fn flush(
    out: &mut Vec<Bar>,
    open: i64,
    f: Bar,
    cnt: i64,
    src_tf_s: u32,
    tf_s: u32,
    st: &mut BuildStats,
) {
    let need = barclock::bars_between(open, barclock::close_ms(open, tf_s), src_tf_s);
    if cnt == need {
        push_row(out, f, tf_s, st);
    } else {
        st.dropped_partial += 1;
    }
}

/// Trade prints sorted by `(t_ms, seq)` → rows of `tf_s`, bucketed by `barclock::open_ms(t_ms)`;
/// a bucket is kept only when `[open, close)` lies inside one coverage interval (§4.2.4).
// pin:07-coverage-closed: a coverage pair `(first, last)` is the run's `[first, last]` wall span,
// inclusive at both ends (§4.2.4's bracket); a print is inside when `first ≤ t ≤ last`; a bucket
// `[open, close)` is inside when `first ≤ open` and `close ≤ last`.
// pin:07-trades-sorted: prints not sorted by `(t_ms, seq)` are refused `NotAscending { row }`.
pub fn bars_from_trades(
    prints: &[TradePrint],
    tf_s: u32,
    coverage: &[(i64, i64)],
    out: &mut Vec<Bar>,
) -> Result<BuildStats, BarFileErr> {
    out.clear();
    if !barclock::is_allowed(tf_s) {
        return Err(BarFileErr::BadTimeframe(tf_s));
    }
    let mut st = BuildStats::default();
    let mut cur: Option<Bar> = None;
    let mut i = 0;
    while i < prints.len() {
        let p = &prints[i];
        let row = i as u64;
        if i > 0 {
            let q = &prints[i - 1];
            if (p.t_ms, p.seq) < (q.t_ms, q.seq) {
                return Err(BarFileErr::NotAscending { row });
            }
        }
        if !coverage.iter().any(|&(a, b)| a <= p.t_ms && p.t_ms <= b) {
            return Err(BarFileErr::OutsideCoverage { row });
        }
        let bo = barclock::open_ms(p.t_ms, tf_s);
        let vol = p.qty_1e6.saturating_abs();
        match &mut cur {
            Some(f) if f.open_ms == bo => {
                f.high_1e6 = f.high_1e6.max(p.px_1e6);
                f.low_1e6 = f.low_1e6.min(p.px_1e6);
                f.close_1e6 = p.px_1e6;
                f.volume_1e6 = f.volume_1e6.saturating_add(vol);
            }
            _ => {
                if let Some(f) = cur.take() {
                    flush_trades(out, f, tf_s, coverage, &mut st);
                }
                cur = Some(Bar {
                    open_1e6: p.px_1e6,
                    high_1e6: p.px_1e6,
                    low_1e6: p.px_1e6,
                    close_1e6: p.px_1e6,
                    volume_1e6: vol,
                    open_ms: bo,
                    ..Bar::default()
                });
            }
        }
        i += 1;
    }
    if let Some(f) = cur.take() {
        flush_trades(out, f, tf_s, coverage, &mut st);
    }
    if out.is_empty() {
        return Err(BarFileErr::Empty);
    }
    st.holes = validate_rows(out, tf_s)?;
    st.rows = out.len() as u64;
    Ok(st)
}

fn flush_trades(
    out: &mut Vec<Bar>,
    f: Bar,
    tf_s: u32,
    coverage: &[(i64, i64)],
    st: &mut BuildStats,
) {
    let close = barclock::close_ms(f.open_ms, tf_s);
    if coverage.iter().any(|&(a, b)| a <= f.open_ms && close <= b) {
        push_row(out, f, tf_s, st);
    } else {
        st.dropped_partial += 1;
    }
}

/// Whether `write_bar_file` wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// A new or changed file was published (tmp + fsync + rename).
    Written,
    /// The existing file already holds exactly these rows (same header, digest included).
    Unchanged,
}

/// Validate, then publish atomically: `<path>.tmp`, `fsync`, `rename(2)` over `path`. Skips the
/// write when the existing file's header equals the new one unless `force` (I5).
// pin:07-write-mkdir: the parent directories (`<root>/<venue>/<symbol>/`) are created when absent.
pub fn write_bar_file(
    path: &Path,
    rows: &[Bar],
    tf_s: u32,
    symbol_hash: u64,
    venue: u8,
    source: u8,
    force: bool,
) -> Result<WriteOutcome, BarFileErr> {
    let io = |e: std::io::Error| BarFileErr::Io(e.raw_os_error().unwrap_or(-1));
    let holes = validate_rows(rows, tf_s)?;
    let hdr = BarFileHdr::of_rows(rows, holes, tf_s, symbol_hash, venue, source);
    if !force {
        if let Ok(mut f) = std::fs::File::open(path) {
            let mut hb = [0u8; 64];
            if std::io::Read::read_exact(&mut f, &mut hb).is_ok()
                && BarFileHdr::from_le_bytes(&hb) == hdr
            {
                return Ok(WriteOutcome::Unchanged);
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut body = Vec::with_capacity(64 + 64 * rows.len());
    body.extend_from_slice(&hdr.to_le_bytes());
    let mut i = 0;
    while i < rows.len() {
        body.extend_from_slice(&row_bytes(&rows[i]));
        i += 1;
    }
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(e));
    }
    Ok(WriteOutcome::Written)
}
