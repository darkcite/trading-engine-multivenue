// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # The long-tenor state rows — one grammar, one reader, one writer (HAR H3.4)
//!
//! A [`LongVolEngine`]'s restorable state as text: the SEED the worker cuts
//! from candles.db (`claude_worker.har_seed seed-out`, one
//! `seed-<NAME>.tsv` per series) and the STATE the engine writes itself
//! (`state-<NAME>.tsv`, at every day close and at shutdown) are the same
//! rows. Tab-separated, `#` lines are comments, applied in file order:
//!
//! ```text
//! V 1
//! D <day_ts_ms> <sum_sq> <n_min>                                  closed days, oldest first
//! C <day_ts_ms> <sum_sq> <n_min> <last_min_ts_ms> <prev_px_1e6>  the open day (at most one)
//! A <tau_days> <day_ts_ms> <x_1e9> <fit_1e9|->                   the arms still pending
//! P <tau_days> <target_day_ts_ms> <x_1e9> <y_1e9>                pairs, oldest first
//! Q <tau_days> <raw_1e9> <fit_1e9>                               QLIKE rows, oldest first
//! ```
//!
//! [`LongVolEngine::write_rows`] is the writer — byte for byte the
//! Python mirror's `har_seed.seed_rows` (pinned by the `long-*` parity
//! fixtures' `R` op); [`parse_rows`] is the reader; [`apply_rows`] puts a
//! parsed set back through the engine's `seed_*` entry points (each is
//! refused, never repaired, and counted), and [`merge_rows`] is the boot's
//! day-merge law (vault plan §6) over a seed and a state.
//!
//! BOOT/COLD DOCTRINE: the parser, the merge and the applier run at boot;
//! the writer runs on the state writer's cold cadence into a reused
//! buffer. Allocation is fine here — nothing in this module is reachable
//! from the tick or timer path.

use std::collections::BTreeMap;

use crate::long::{LongVolEngine, DAY_MS, DAY_NS, DAY_RING, LONG_TAU_DAYS_MAX, PAIR_RING_LONG};
use crate::QLIKE_RING;

/// The row grammar's version (`V 1`).
pub const ROWS_VERSION: u32 = 1;

/// "No fit when armed" in an `A` row (`-` on the wire).
const NONE: i64 = i64::MIN;

/// The open day: `(day_ts_ms, sum_sq, n_min, last_min_ts_ms, prev_px_1e6)`.
pub type OpenRow = (u64, i128, u32, u64, i64);

/// One file's rows, typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LongRows {
    /// `D`: `(day_ts_ms, sum_sq, n_min)`, oldest first.
    pub days: Vec<(u64, i128, u32)>,
    /// `C`: the open day.
    pub open: Option<OpenRow>,
    /// `A`: `(tau_days, day_ts_ms, x_1e9, fit_1e9)` — `fit = i64::MIN`: none.
    pub arms: Vec<(u32, u64, i64, i64)>,
    /// `P`: `(tau_days, target_day_ts_ms, x_1e9, y_1e9)`, oldest first per tenor.
    pub pairs: Vec<(u32, u64, i64, i64)>,
    /// `Q`: `(tau_days, raw_1e9, fit_1e9)`, oldest first per tenor.
    pub qlike: Vec<(u32, i64, i64)>,
}

impl LongRows {
    /// The newest minute the rows reach: the open day's last minute, or
    /// the end of the newest closed day; `0` when empty.
    #[must_use]
    pub fn last_min_ts_ms(&self) -> u64 {
        match (self.open, self.days.last()) {
            (Some(o), _) => o.3,
            (None, Some(d)) => d.0 + DAY_MS - 60_000,
            (None, None) => 0,
        }
    }
}

/// A row that did not hold: the 1-based line and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowsErr {
    /// 1-based line number (`0`: the file as a whole).
    pub line: usize,
    /// What was wrong.
    pub why: String,
}

impl core::fmt::Display for RowsErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.why)
        } else {
            write!(f, "line {}: {}", self.line, self.why)
        }
    }
}

impl std::error::Error for RowsErr {}

fn bad(line: usize, why: impl Into<String>) -> RowsErr {
    RowsErr {
        line,
        why: why.into(),
    }
}

fn num<T: core::str::FromStr>(s: &str, line: usize, what: &str) -> Result<T, RowsErr> {
    s.parse::<T>()
        .map_err(|_| bad(line, format!("`{what}` is not an integer: `{s}`")))
}

fn tau_days(s: &str, line: usize) -> Result<u32, RowsErr> {
    let d: u32 = num(s, line, "tau_days")?;
    if d == 0 || d as usize > LONG_TAU_DAYS_MAX {
        return Err(bad(line, format!("tau_days {d} is off the 1..=40 grid")));
    }
    Ok(d)
}

/// Parse one file's rows. Strict: `V 1` first, each row its exact width,
/// integers only (`-` only as an `A` row's fit), `D` rows before the one
/// `C`, `C` before any `A`/`P`/`Q`, tenors on the grid.
pub fn parse_rows(src: &str) -> Result<LongRows, RowsErr> {
    let mut rows = LongRows::default();
    let mut seen_v = false;
    let mut past_days = false;
    for (idx, raw) in src.lines().enumerate() {
        let line = idx + 1;
        let l = raw.trim_end_matches('\r');
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = l.split('\t').collect();
        let width = match f[0] {
            "V" => 2,
            "D" | "Q" => 4,
            "C" => 6,
            "A" | "P" => 5,
            other => return Err(bad(line, format!("unknown row `{other}`"))),
        };
        if f.len() != width {
            return Err(bad(line, format!("`{}` rows carry {} fields, got {}", f[0], width, f.len())));
        }
        if !seen_v {
            if f[0] != "V" {
                return Err(bad(line, "the first row must be `V 1`"));
            }
            let v: u32 = num(f[1], line, "version")?;
            if v != ROWS_VERSION {
                return Err(bad(line, format!("version {v}, this reader reads {ROWS_VERSION}")));
            }
            seen_v = true;
            continue;
        }
        match f[0] {
            "V" => return Err(bad(line, "a second `V` row")),
            "D" => {
                if past_days {
                    return Err(bad(line, "a `D` row after the open day or the arms"));
                }
                rows.days.push((
                    num(f[1], line, "day_ts_ms")?,
                    num(f[2], line, "sum_sq")?,
                    num(f[3], line, "n_min")?,
                ));
            }
            "C" => {
                if rows.open.is_some() || !rows.arms.is_empty() || !rows.pairs.is_empty() || !rows.qlike.is_empty() {
                    return Err(bad(line, "a `C` row must be the only one, before the arms"));
                }
                past_days = true;
                rows.open = Some((
                    num(f[1], line, "day_ts_ms")?,
                    num(f[2], line, "sum_sq")?,
                    num(f[3], line, "n_min")?,
                    num(f[4], line, "last_min_ts_ms")?,
                    num(f[5], line, "prev_px_1e6")?,
                ));
            }
            "A" => {
                past_days = true;
                let fit = if f[4] == "-" { NONE } else { num(f[4], line, "fit_1e9")? };
                rows.arms.push((
                    tau_days(f[1], line)?,
                    num(f[2], line, "day_ts_ms")?,
                    num(f[3], line, "x_1e9")?,
                    fit,
                ));
            }
            "P" => {
                past_days = true;
                rows.pairs.push((
                    tau_days(f[1], line)?,
                    num(f[2], line, "target_day_ts_ms")?,
                    num(f[3], line, "x_1e9")?,
                    num(f[4], line, "y_1e9")?,
                ));
            }
            _ => {
                past_days = true;
                rows.qlike.push((
                    tau_days(f[1], line)?,
                    num(f[2], line, "raw_1e9")?,
                    num(f[3], line, "fit_1e9")?,
                ));
            }
        }
    }
    if !seen_v {
        return Err(bad(0, "no `V` row: not a long-tenor rows file"));
    }
    Ok(rows)
}

/// What [`apply_rows`] put back, and what the engine refused.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyStats {
    /// Rows the engine accepted.
    pub applied: u32,
    /// Rows the engine refused (counted, never repaired).
    pub refused: u32,
}

/// Put `rows` back into a FRESH engine through its `seed_*` entry points,
/// in the grammar's order. The caller refreshes once afterwards (the set's
/// `restored`).
pub fn apply_rows(e: &mut LongVolEngine, rows: &LongRows) -> ApplyStats {
    let mut st = ApplyStats::default();
    let mut count = |ok: bool| {
        if ok {
            st.applied += 1;
        } else {
            st.refused += 1;
        }
    };
    for &(ts, sq, n) in &rows.days {
        count(e.seed_day(ts, sq, n));
    }
    if let Some((ts, sq, n, last, prev)) = rows.open {
        count(e.seed_open(ts, sq, n, last, prev));
    }
    for &(d, ts, x, fit) in &rows.arms {
        count(e.seed_arm(d as u64 * DAY_NS, ts, x, fit));
    }
    for &(d, ts, x, y) in &rows.pairs {
        count(e.seed_pair(d as u64 * DAY_NS, ts, x, y));
    }
    for &(d, raw, fit) in &rows.qlike {
        count(e.seed_qlike(d as u64 * DAY_NS, raw, fit));
    }
    st
}

/// The boot's day-merge law (vault plan §6) over the worker's SEED and the
/// engine's own STATE, either of which may be absent:
///
/// * `D`, keyed by day: the day with MORE minutes wins, a tie keeps the
///   state's (a short day in a restarted engine's state must not beat a
///   complete day in the seed); the union is made contiguous with EMPTY
///   days and cut to the newest [`DAY_RING`];
/// * `C`: the one with the later `last_min_ts_ms` wins — the only row that
///   says where the next minute continues from — if its day follows the
///   newest closed day (the gap between them filled EMPTY); a `C` at or
///   before the newest closed day is stale and dropped for the other;
/// * `A`, keyed (tenor, day), and `P`, keyed (tenor, target day): the
///   union, the state winning a shared key (`A` only for resident days;
///   `P` the newest [`PAIR_RING_LONG`] per tenor);
/// * `Q` (no key but its slot): per tenor, the state's rows when it has
///   any, else the seed's — two chronologies never interleave.
#[must_use]
pub fn merge_rows(seed: Option<&LongRows>, state: Option<&LongRows>) -> LongRows {
    let empty = LongRows::default();
    let (seed, state) = match (seed, state) {
        (None, None) => return empty,
        (Some(s), None) => (s, &empty),
        (None, Some(t)) => (&empty, t),
        (Some(s), Some(t)) => (s, t),
    };
    let mut days: BTreeMap<u64, (i128, u32)> = BTreeMap::new();
    for &(ts, sq, n) in &seed.days {
        days.insert(ts, (sq, n));
    }
    for &(ts, sq, n) in &state.days {
        match days.get(&ts) {
            Some(&(_, seed_n)) if seed_n > n => {}
            _ => {
                days.insert(ts, (sq, n));
            }
        }
    }
    let mut out = LongRows::default();
    if let (Some(&first), Some(&last)) = (days.keys().next(), days.keys().next_back()) {
        let mut ts = first;
        while ts <= last {
            let (sq, n) = days.get(&ts).copied().unwrap_or((0, 0));
            out.days.push((ts, sq, n));
            ts += DAY_MS;
        }
    }
    // The open day: the later last minute, if it can follow the days.
    let mut candidates = [state.open, seed.open];
    if let (Some(a), Some(b)) = (candidates[0], candidates[1]) {
        if b.3 > a.3 {
            candidates.swap(0, 1);
        }
    }
    let newest_day = out.days.last().map(|d| d.0);
    for c in candidates.into_iter().flatten() {
        match newest_day {
            Some(nd) if c.0 <= nd => continue,
            Some(nd) => {
                let mut ts = nd + DAY_MS;
                while ts < c.0 {
                    out.days.push((ts, 0, 0));
                    ts += DAY_MS;
                }
            }
            None => {}
        }
        out.open = Some(c);
        break;
    }
    if out.days.len() > DAY_RING {
        out.days.drain(..out.days.len() - DAY_RING);
    }
    let resident: std::collections::BTreeSet<u64> = out.days.iter().map(|d| d.0).collect();
    let mut arms: BTreeMap<(u32, u64), (i64, i64)> = BTreeMap::new();
    for rows in [seed, state] {
        for &(d, ts, x, fit) in &rows.arms {
            if resident.contains(&ts) {
                arms.insert((d, ts), (x, fit));
            }
        }
    }
    out.arms = arms.into_iter().map(|((d, ts), (x, fit))| (d, ts, x, fit)).collect();
    let mut pairs: BTreeMap<(u32, u64), (i64, i64)> = BTreeMap::new();
    for rows in [seed, state] {
        for &(d, ts, x, y) in &rows.pairs {
            pairs.insert((d, ts), (x, y));
        }
    }
    for d in 1..=LONG_TAU_DAYS_MAX as u32 {
        let tenor: Vec<(u32, u64, i64, i64)> = pairs
            .range((d, 0)..=(d, u64::MAX))
            .map(|(&(d, ts), &(x, y))| (d, ts, x, y))
            .collect();
        let skip = tenor.len().saturating_sub(PAIR_RING_LONG);
        out.pairs.extend_from_slice(&tenor[skip..]);
        let from_state: Vec<(u32, i64, i64)> =
            state.qlike.iter().copied().filter(|q| q.0 == d).collect();
        let src = if from_state.is_empty() {
            seed.qlike.iter().copied().filter(|q| q.0 == d).collect()
        } else {
            from_state
        };
        let skip = src.len().saturating_sub(QLIKE_RING);
        out.qlike.extend_from_slice(&src[skip..]);
    }
    out
}

impl LongVolEngine {
    /// Write the engine's restorable state as rows (module doc) into `w`:
    /// `V`, every resident day, the open day, the arms still pending (made
    /// at one of the last `τ` closes), then per tenor its pairs and its
    /// QLIKE rows — byte for byte what `claude_worker.har_seed.seed_rows`
    /// writes for the same engine.
    pub fn write_rows<W: core::fmt::Write>(&self, w: &mut W) -> core::fmt::Result {
        writeln!(w, "V\t{ROWS_VERSION}")?;
        let n_res = self.n_resident();
        let mut i = 0usize;
        while let Some((ts, sq, n)) = self.day_at(i) {
            writeln!(w, "D\t{ts}\t{sq}\t{n}")?;
            i += 1;
        }
        if let Some((ts, sq, n)) = self.open_day() {
            writeln!(
                w,
                "C\t{ts}\t{sq}\t{n}\t{}\t{}",
                self.last_min_ts_ms(),
                self.prev_px_1e6()
            )?;
        }
        let mut d = 1usize;
        while d <= LONG_TAU_DAYS_MAX {
            let tau = d as u64 * DAY_NS;
            let mut i = n_res.saturating_sub(d);
            while i < n_res {
                if let (Some((x, fit)), Some((ts, _, _))) = (self.arm_at(i, tau), self.day_at(i)) {
                    if fit == NONE {
                        writeln!(w, "A\t{d}\t{ts}\t{x}\t-")?;
                    } else {
                        writeln!(w, "A\t{d}\t{ts}\t{x}\t{fit}")?;
                    }
                }
                i += 1;
            }
            d += 1;
        }
        let mut d = 1usize;
        while d <= LONG_TAU_DAYS_MAX {
            let tau = d as u64 * DAY_NS;
            let mut i = 0usize;
            while let Some((ts, x, y)) = self.pair_at(tau, i) {
                writeln!(w, "P\t{d}\t{ts}\t{x}\t{y}")?;
                i += 1;
            }
            let mut i = 0usize;
            while let Some((raw, fit)) = self.qlike_at(tau, i) {
                writeln!(w, "Q\t{d}\t{raw}\t{fit}")?;
                i += 1;
            }
            d += 1;
        }
        Ok(())
    }
}

/// The rows of `e`, typed (the writer's output through the reader).
#[must_use]
pub fn rows_of(e: &LongVolEngine) -> LongRows {
    let mut text = String::new();
    // A `String` sink cannot fail.
    let _ = e.write_rows(&mut text);
    parse_rows(&text).unwrap_or_default()
}

#[cfg(test)]
mod tests;
