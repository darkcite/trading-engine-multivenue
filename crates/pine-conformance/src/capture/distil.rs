// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Export in, `.fix` out (impl-06 §3.10, §4.7.5): `distil` (a sheet's chart export, optionally a
//! strategy's trades and summary) and `record_paste` (a fixture pasted alone). `verify = true`
//! compares instead of writing (the re-verification session). Offline capture tooling.

use std::path::{Path, PathBuf};

use pine_ir::Dialect;

use super::csv::ChartExport;
use crate::chart::spec;
use crate::fixture::{
    is_capture_utc, is_tv_build, Align, Cell, Dialects, Driver, Expect, Fixture, FixtureId, Group,
    ReadingBody, Row, Stamp, Summary, Tol, TradeRow, Window,
};

/// `utc` is the canonical capture stamp `YYYY-MM-DDTHH:MMZ` (§4.4.1, `fixture::is_capture_utc`).
#[derive(Debug, Clone)]
pub struct CaptureStamp {
    /// When.
    pub utc: String,
    /// `tv_build`.
    pub tv_build: String,
    /// The dialect pasted.
    pub dialect: Dialect,
}

/// What a distil (or a verify) touched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DistilReport {
    /// Fixtures written (or, under `verify`, compared equal).
    pub written: Vec<FixtureId>,
    /// Under `verify`: fixtures whose new body differs from the tracked one.
    pub differing: Vec<FixtureId>,
    /// Fixtures refused and not written, with the reason: a second-dialect capture that differs
    /// from the first (I9, `pin:06-distil-56`). The rest of the sheet is still written.
    pub refused: Vec<(FixtureId, String)>,
}

/// What TradingView did when one fixture was pasted alone (§4.7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paste {
    /// Compiled and ran.
    Compiled,
    /// Refused at compile time.
    Refused,
    /// A runtime error.
    RuntimeError,
}

/// `pin:06-distil-root` — §3.10's `distil` takes a sheet path, no root: the workspace root is the
/// nearest ancestor of the sheet (or `.fix`) holding `tests/conformance/fixtures`.
fn root_of(p: &Path) -> Result<PathBuf, String> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_err(|e| e.to_string())?.join(p)
    };
    abs.ancestors()
        .find(|a| a.join("tests/conformance/fixtures").is_dir())
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("{}: no tests/conformance/fixtures above it", p.display()))
}

fn fix_path(root: &Path, id: FixtureId) -> PathBuf {
    root.join(format!(
        "tests/conformance/fixtures/{}/{}.fix",
        id.group().letter(),
        id.as_str()
    ))
}

fn load(path: &Path) -> Result<Fixture, String> {
    // COPY: one `.fix` file's text (≤ the I18 payload ratchet), offline — the parser reads owned
    // text — a memory map rejected (no third-party crate, contract §2).
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Fixture::parse(&text, &path.display().to_string()).map_err(|e| e.to_string())
}

fn check_stamp(s: &CaptureStamp) -> Result<(), String> {
    if !is_capture_utc(&s.utc) {
        return Err(format!("`{}` is not YYYY-MM-DDTHH:MMZ", s.utc));
    }
    if !is_tv_build(&s.tv_build) {
        return Err(format!("`{}` is not a tv_build", s.tv_build));
    }
    Ok(())
}

const fn dialect_bit(d: Dialect) -> Dialects {
    match d {
        Dialect::V5 => Dialects::V5,
        Dialect::V6 => Dialects::V6,
    }
}

/// The fixture id a plot title belongs to (`f_<ID>_…`, I4).
fn title_id(t: &str) -> Option<FixtureId> {
    let rest = t.strip_prefix("f_")?;
    let id = FixtureId::parse(rest.get(..4)?)?;
    rest[4..].starts_with('_').then_some(id)
}

/// Whether an exported cell is the plotted integer `1`.
fn is_one(c: &Cell) -> bool {
    matches!(c, Cell::Val { v, .. } if *v == 1.0)
}

/// The rows a window keeps of `n` eligible rows: `[lo, hi)` positions among them.
///
/// `pin:06-distil-short-window` — a `head n` / `tail n` body holds exactly `n` rows
/// (`pin:06-window-row-count`), so fewer eligible rows than `n` is `Err`: the distiller never writes
/// a `.fix` its own parser refuses.
fn head_tail(id: FixtureId, w: Window, n: usize, what: &str) -> Result<(usize, usize), String> {
    let (k, head) = match w {
        Window::Head(k) => (k as usize, true),
        Window::Tail(k) => (k as usize, false),
        Window::Time { .. } => return Ok((0, n)),
    };
    if n < k {
        return Err(format!(
            "{}: window wants {k} row(s), the export has {n} {what} (pin:06-distil-short-window)",
            id.as_str()
        ));
    }
    Ok(if head { (0, k) } else { (n - k, n) })
}

/// Whether `f` declares both realtime flag plots (`f_<ID>_rt`, `f_<ID>_done`).
fn has_rt_flags(f: &Fixture) -> bool {
    let want = |s: &str| {
        let t = format!("f_{}_{s}", f.id.as_str());
        f.plots.iter().any(|p| p.title == t)
    };
    want("rt") && want("done")
}

/// `pin:06-distil-tick-columns` — the plot suffixes (`f_<ID>_<suffix>`) of a `driver rt` fixture that
/// are per-update bookkeeping, never a relation: the row filter (`rt`, `done`) and the tick counts
/// (`upd`, `k`). §4.7.8: "tick-count columns are never compared" (R-P3).
pub const RT_BOOKKEEPING: &[&str] = &["rt", "done", "upd", "k"];

/// `pin:06-distil-tick-columns` — the generator marks any further tick-count plot by this suffix
/// prefix (`f_<ID>_tick_<what>`); such a column is bookkeeping too.
pub const TICK_COUNTER_PREFIX: &str = "tick_";

/// Whether plot `title` of fixture `id` is a relation column (§4.7.8): every plot of a flagged
/// `driver rt` fixture except [`RT_BOOKKEEPING`] and the [`TICK_COUNTER_PREFIX`] columns.
pub fn is_relation_column(id: FixtureId, title: &str) -> bool {
    let sfx = title
        .strip_prefix("f_")
        .and_then(|r| r.strip_prefix(id.as_str()))
        .and_then(|r| r.strip_prefix('_'))
        .unwrap_or(title);
    !(RT_BOOKKEEPING.contains(&sfx) || sfx.starts_with(TICK_COUNTER_PREFIX))
}

/// The §4.7.8 `relation` body over the kept `rt = done = 1` rows: per relation column its one
/// value. `pin:06-distil-relation` — a column that takes more than one value over those rows, or
/// whose one value is `na` or not an integer, is not a tick-independent relation: `Err` naming the
/// column and its distinct values in row order (an I9-style refusal — never averaged).
fn relation_of(f: &Fixture, rows: &[Row]) -> Result<Vec<(String, i64)>, String> {
    let id = f.id.as_str();
    let mut out = Vec::new();
    for (p, spec) in f.plots.iter().enumerate() {
        if !is_relation_column(f.id, &spec.title) {
            continue;
        }
        let mut distinct: Vec<&Cell> = Vec::new();
        for r in rows {
            let c = &r.cells[p];
            let seen = distinct.iter().any(|d| match (d, c) {
                (Cell::Na, Cell::Na) => true,
                (Cell::Val { v: a, .. }, Cell::Val { v: b, .. }) => a == b,
                _ => false,
            });
            if !seen {
                distinct.push(c);
            }
        }
        let shown = |d: &[&Cell]| -> String {
            d.iter()
                .map(|c| match c {
                    Cell::Na => "na".to_string(),
                    Cell::Val { text, .. } => text.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        match distinct.as_slice() {
            [Cell::Val { v, .. }] if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e18 => {
                out.push((spec.title.clone(), *v as i64));
            }
            [one] => {
                return Err(format!(
                    "{id}: relation column `{}` is constant but not an integer ({}) — §4.7.8 \
                     (pin:06-distil-relation)",
                    spec.title,
                    shown(&[one])
                ))
            }
            [] => {
                return Err(format!(
                    "{id}: no rt = done = 1 row to read relation column `{}` from",
                    spec.title
                ))
            }
            many => {
                let (n, k, vals) = (rows.len(), many.len(), shown(many));
                let why = format!(
                    "{id}: relation column `{}` varies over the {n} rt = done = 1 rows ({k} \
                     distinct: {vals}) — not tick-independent; refused, never averaged (§4.7.8, \
                     pin:06-distil-relation)",
                    spec.title
                );
                return Err(why);
            }
        }
    }
    if out.is_empty() {
        return Err(format!(
            "{id}: a driver-rt fixture with rt / done plots but no relation column \
             (pin:06-distil-tick-columns)"
        ));
    }
    Ok(out)
}

/// The export column of `f`'s plot titled `f_<ID>_<suffix>`, or `Err` naming the fixture.
fn rt_col(f: &Fixture, cols: &[usize], suffix: &str) -> Result<usize, String> {
    let title = format!("f_{}_{suffix}", f.id.as_str());
    f.plots
        .iter()
        .position(|p| p.title == title)
        .map(|p| cols[p])
        .ok_or_else(|| {
            format!(
                "{}: a driver-rt fixture without its `{title}` plot cannot be distilled — §4.7.8 keeps \
                 the rows where `f_<ID>_rt` and `f_<ID>_done` are 1 (pin:06-distil-rt-rows)",
                f.id.as_str()
            )
        })
}

/// Rows of the export selected by the fixture's alignment and window (§4.1.3), and the `bars` to
/// record (`None` = leave `f.bars` as it is).
///
/// - `align step`: TradingView's last row is the open realtime bar and is dropped; `bars` = the
///   remaining row count; `head n` keeps rows `0..n`, `tail n` rows `bars−n..bars`, the key the row
///   index (bar 0 = TradingView's first loaded bar).
/// - `align time`: only rows whose bar open lies in `[FROM_MS, TO_MS)` (`pine_spec::chart`) are
///   eligible; `head n` / `tail n` select among them, a `time` window intersects with that range;
///   the key is the bar open ms.
/// - `driver rt` (§4.7.8): only the rows where the fixture's `f_<ID>_rt` and `f_<ID>_done` plots are
///   both 1 are eligible (the open realtime bar has `done = 0` and drops out with them); `tail n`
///   selects among them; the keys are `bars−n..bars` with `bars = history + realtime` of the `rt`
///   line — the rows `run.rs` steps for an `Rt` drive (§4.1.6), whose last `realtime` rows are the
///   realtime bars. A `driver rt` fixture without the two flag plots (`pin:06-distil-rt-noflags`,
///   C045 and E106) is selected by position instead: with `open` on its `rt` line the export's last
///   rows including the open bar are the compared bars (`n ≤ bars`), without it the open last row is
///   dropped and the last confirmed rows are (`n ≤ realtime`); the keys are `bars−n..bars`.
///
/// `pin:06-distil-rt-rows` — the rt rule applies to `driver rt` fixtures (the live session of
/// §4.7.8); a `mode realtime` fixture on another driver (E016–E032, E105: on-tick / on-fill
/// strategies over history) is distilled by its alignment. A `driver rt` fixture needs a `tail n`
/// window with `n ≤ realtime` (the compared keys are realtime bars on our side).
fn select(
    f: &Fixture,
    export: &ChartExport,
    cols: &[usize],
) -> Result<(Vec<Row>, Option<u32>), String> {
    let id = f.id;
    let rows = &export.rows;
    // (export row index, key) of every selected row.
    let mut picked: Vec<(usize, i64)> = Vec::new();
    let bars;
    if f.driver == Driver::Rt {
        let rt =
            f.rt.ok_or_else(|| format!("{}: driver rt without an `rt` line", id.as_str()))?;
        let total = rt.history + rt.realtime;
        if !has_rt_flags(f) {
            // pin:06-distil-rt-noflags — C045 (`rt … open`) and E106 carry no rt / done plot. The
            // §4.7.8 procedure fixes which rows are compared: an `open` fixture is exported before
            // its first realtime bar closes, so its last `n` rows are the history tail plus the open
            // realtime bar (kept, keys `total−n..total`); any other is exported after at least
            // `realtime` realtime bars closed, so the open last row is dropped and the last `n`
            // confirmed rows are its realtime bars. The export itself cannot prove either.
            let k = match f.window {
                Window::Tail(k) if (rt.open && k <= total) || (!rt.open && k <= rt.realtime) => k,
                _ => {
                    return Err(format!(
                        "{}: a driver-rt fixture without rt / done plots compares `tail n` with \
                         n ≤ {} (pin:06-distil-rt-noflags)",
                        id.as_str(),
                        if rt.open { total } else { rt.realtime }
                    ))
                }
            };
            let n = if rt.open {
                rows.len()
            } else {
                rows.len().checked_sub(1).ok_or_else(|| {
                    format!(
                        "{}: an empty export (no open realtime bar to drop)",
                        id.as_str()
                    )
                })?
            };
            let (lo, hi) = head_tail(id, f.window, n, "row(s)")?;
            let first = i64::from(total) - i64::from(k);
            for (j, i) in (lo..hi).enumerate() {
                picked.push((i, first + j as i64));
            }
            let out = picked
                .iter()
                .map(|&(i, key)| Row {
                    key,
                    cells: cols.iter().map(|c| rows[i].1[*c].clone()).collect(),
                })
                .collect();
            return Ok((out, Some(total)));
        }
        let (c_rt, c_done) = (rt_col(f, cols, "rt")?, rt_col(f, cols, "done")?);
        let k = match f.window {
            Window::Tail(k) if k <= rt.realtime => k,
            _ => {
                return Err(format!(
                    "{}: a driver-rt fixture compares `tail n` with n ≤ realtime {} \
                     (pin:06-distil-rt-rows)",
                    id.as_str(),
                    rt.realtime
                ))
            }
        };
        let elig: Vec<usize> = (0..rows.len())
            .filter(|i| is_one(&rows[*i].1[c_rt]) && is_one(&rows[*i].1[c_done]))
            .collect();
        let (lo, hi) = head_tail(
            id,
            f.window,
            elig.len(),
            "realtime row(s) with rt = done = 1",
        )?;
        let first = i64::from(total) - i64::from(k);
        for (j, &i) in elig[lo..hi].iter().enumerate() {
            picked.push((i, first + j as i64));
        }
        bars = Some(total);
    } else {
        match f.align {
            Align::Step => {
                let n = rows.len().checked_sub(1).ok_or_else(|| {
                    format!(
                        "{}: an empty export (no open realtime bar to drop)",
                        id.as_str()
                    )
                })?;
                let (lo, hi) = head_tail(id, f.window, n, "confirmed row(s)")?;
                for (i, (t, _)) in rows.iter().enumerate().take(hi).skip(lo) {
                    let in_window = match f.window {
                        Window::Time { from_ms, to_ms } => *t >= from_ms && *t < to_ms,
                        _ => true,
                    };
                    if in_window {
                        picked.push((i, i as i64));
                    }
                }
                bars = Some(n as u32);
            }
            Align::Time => {
                let (from, to) = match f.window {
                    Window::Time { from_ms, to_ms } => {
                        (from_ms.max(spec::FROM_MS), to_ms.min(spec::TO_MS))
                    }
                    _ => (spec::FROM_MS, spec::TO_MS),
                };
                let elig: Vec<usize> = (0..rows.len())
                    .filter(|i| rows[*i].0 >= from && rows[*i].0 < to)
                    .collect();
                let (lo, hi) = head_tail(id, f.window, elig.len(), "row(s) in [FROM_MS, TO_MS)")?;
                for &i in &elig[lo..hi] {
                    picked.push((i, rows[i].0));
                }
                bars = None;
            }
        }
    }
    let out = picked
        .iter()
        .map(|&(i, key)| Row {
            key,
            cells: cols.iter().map(|c| rows[i].1[*c].clone()).collect(),
        })
        .collect();
    Ok((out, bars))
}

/// §4.4.2: an export of fewer than 17 digits turns every non-integral `exact` / `ulp` plot into
/// `dec <digits>`.
fn retol(f: &mut Fixture, rows: &[Row], digits: u8) {
    if digits >= 17 {
        return;
    }
    for (p, spec) in f.plots.iter_mut().enumerate() {
        let integral = rows.iter().all(|r| match &r.cells[p] {
            Cell::Na => true,
            Cell::Val { v, .. } => !v.is_finite() || v.fract() == 0.0,
        });
        if !integral && matches!(spec.tol, Tol::Exact | Tol::Ulp(_)) {
            spec.tol = Tol::Dec(digits);
        }
    }
}

/// `pin:06-distil-56` — a `56` fixture's second dialect capture must reproduce the first exactly
/// (I9: TradingView's v5 and v6 agree, or the fixture is mis-tabled): a differing body is refused —
/// the reason, collected in [`DistilReport::refused`]; that fixture is not written, the rest of the
/// sheet is (one mis-tabled fixture never stops a sheet). Under `verify` nothing is written and the
/// tracked body is the reference for both dialects: a difference is `differing`, not a refusal.
fn i9_refusal(f: &Fixture, body: &Expect, stamp: &CaptureStamp) -> Option<String> {
    let bit = dialect_bit(stamp.dialect);
    let prior = f.captured_in.filter(|c| c.0 & !bit.0 != 0);
    // A capture outside the fixture's dialects is `apply`'s `Err`, never a refusal.
    (f.dialects.has(stamp.dialect) && prior.is_some() && f.expect != *body).then(|| {
        format!(
            "{}: the v{} capture differs from the other dialect's (I9, pin:06-distil-56)",
            f.id.as_str(),
            stamp.dialect as u8
        )
    })
}

/// Applies a new capture to `f`; returns whether the tracked file changes in its body. The caller
/// has already ruled out an I9 refusal ([`i9_refusal`]).
fn apply(
    f: &mut Fixture,
    body: Expect,
    stamp: &CaptureStamp,
    bars: Option<u32>,
) -> Result<bool, String> {
    let bit = dialect_bit(stamp.dialect);
    if !f.dialects.has(stamp.dialect) {
        return Err(format!(
            "{}: captured in v{} outside its dialects",
            f.id.as_str(),
            stamp.dialect as u8
        ));
    }
    let changed = f.expect != body;
    f.expect = body;
    f.captured = Stamp::At(stamp.utc.clone());
    f.tv_build = Stamp::At(stamp.tv_build.clone());
    f.captured_in = Some(Dialects(f.captured_in.map_or(0, |c| c.0) | bit.0));
    if bars.is_some() {
        f.bars = bars;
    }
    Ok(changed)
}

fn finish(
    path: &Path,
    f: &Fixture,
    tracked: &Fixture,
    verify: bool,
    rep: &mut DistilReport,
) -> Result<(), String> {
    if verify {
        if tracked.expect == f.expect {
            rep.written.push(f.id);
        } else {
            rep.differing.push(f.id);
        }
        return Ok(());
    }
    std::fs::write(path, f.render()).map_err(|e| format!("{}: {e}", path.display()))?;
    rep.written.push(f.id);
    Ok(())
}

/// Distils one export into the sheet's fixtures: drops OHLCV and foreign columns (I4), selects the
/// rows by the fixture's alignment, fills `captured` / `tv_build` / `captured_in` / `bars`, writes
/// the expectation. `verify = true` compares instead of writing (the re-verification session).
///
/// The sheet's fixtures are the ids its exported plot titles name (`f_<ID>_…`); `trades` /
/// `summary` belong to a one-fixture strategy sheet.
pub fn distil(
    sheet: &Path,
    export: &ChartExport,
    trades: Option<&[TradeRow]>,
    summary: Option<&Summary>,
    stamp: &CaptureStamp,
    verify: bool,
) -> Result<DistilReport, String> {
    check_stamp(stamp)?;
    let root = root_of(sheet)?;
    let mut ids: Vec<FixtureId> = export.titles.iter().filter_map(|t| title_id(t)).collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err("the export names no fixture plot (f_<ID>_…)".into());
    }
    if (trades.is_some() || summary.is_some()) && ids.len() != 1 {
        return Err("trades and a summary belong to a one-fixture sheet".into());
    }
    let mut rep = DistilReport::default();
    for id in ids {
        let path = fix_path(&root, id);
        let tracked = load(&path)?;
        let mut f = tracked.clone();
        // Foreign / OHLCV columns are never read: each plot picks its own title's column.
        let mut cols = Vec::with_capacity(f.plots.len());
        for p in &f.plots {
            let c = export
                .titles
                .iter()
                .position(|t| *t == p.title)
                .ok_or_else(|| format!("{}: plot `{}` not in the export", id.as_str(), p.title))?;
            cols.push(c);
        }
        let (rows, bars) = select(&f, export, &cols)?;
        // §4.7.8 (R-P3): a flagged `driver rt` fixture records the relation body, not its rows;
        // the no-flags rule (C045, E106 — pin:06-distil-rt-noflags) keeps its rows.
        let relation = f.driver == Driver::Rt && has_rt_flags(&f);
        if relation && (trades.is_some() || summary.is_some()) {
            return Err(format!(
                "{}: a relation fixture carries no trades or summary (§4.7.8)",
                id.as_str()
            ));
        }
        let body = match trades {
            _ if relation => match relation_of(&f, &rows) {
                Ok(rel) => Expect::Relation(rel),
                Err(why) => {
                    rep.refused.push((id, why));
                    continue;
                }
            },
            Some(tr) => Expect::Strategy {
                series: rows.clone(),
                // COPY: the List of Trades rows (one strategy's trades), offline — the fixture owns
                // its expectation — borrowing rejected (the `.fix` is rendered from an owned model).
                trades: tr.to_vec(),
                summary: summary.cloned(),
            },
            None => Expect::Series(rows.clone()),
        };
        if !verify {
            if let Some(why) = i9_refusal(&f, &body, stamp) {
                rep.refused.push((id, why));
                continue;
            }
            retol(&mut f, &rows, export.digits);
        }
        apply(&mut f, body, stamp, bars)?;
        finish(&path, &f, &tracked, verify, &mut rep)?;
    }
    Ok(rep)
}

/// R-P3 for a fixture distilled before the relation rule: a flagged `driver rt` fixture whose
/// captured body is still the `series` rows distil kept (every row `rt = done = 1`) gets the
/// §4.7.8 `relation` body those rows yield — the same [`relation_of`] a fresh distil applies, the
/// stamps unchanged. A varying column is refused (in `refused`, the file untouched); a fixture that
/// is not a flagged rt fixture, or holds no captured `series` body, is `Err`. `verify = true`
/// compares instead of writing.
///
/// `pin:06-distil-rebody` — no TradingView export is re-read: the kept rows already in the `.fix`
/// are exactly the rows distil would keep from it (`tail n` of the `rt = done = 1` rows).
pub fn rebody_relation(fix: &Path, verify: bool) -> Result<DistilReport, String> {
    let tracked = load(fix)?;
    let mut f = tracked.clone();
    let id = f.id;
    if f.driver != Driver::Rt || !has_rt_flags(&f) {
        return Err(format!(
            "{}: not a driver-rt fixture with rt / done plots (§4.7.8)",
            id.as_str()
        ));
    }
    let Expect::Series(rows) = &f.expect else {
        return Err(format!(
            "{}: no captured series body to turn into a relation",
            id.as_str()
        ));
    };
    if !matches!(f.captured, Stamp::At(_)) {
        return Err(format!("{}: not captured", id.as_str()));
    }
    let (c_rt, c_done) = (
        rt_col(&f, &(0..f.plots.len()).collect::<Vec<_>>(), "rt")?,
        rt_col(&f, &(0..f.plots.len()).collect::<Vec<_>>(), "done")?,
    );
    if !rows
        .iter()
        .all(|r| is_one(&r.cells[c_rt]) && is_one(&r.cells[c_done]))
    {
        return Err(format!(
            "{}: a series row with rt or done ≠ 1 is not a kept realtime row",
            id.as_str()
        ));
    }
    let mut rep = DistilReport::default();
    let rel = match relation_of(&f, rows) {
        Ok(rel) => rel,
        Err(why) => {
            rep.refused.push((id, why));
            return Ok(rep);
        }
    };
    f.expect = Expect::Relation(rel);
    if verify {
        // The relation is a re-reading of the tracked rows: equal by construction.
        rep.written.push(id);
        return Ok(rep);
    }
    std::fs::write(fix, f.render()).map_err(|e| format!("{}: {e}", fix.display()))?;
    rep.written.push(id);
    Ok(rep)
}

/// Records a paste: `parse accept|reject` for a group-Z probe; for a pin fixture, the body of its
/// `refuse` (`Refused`) or `fault` (`RuntimeError`) reading plus the stamps (a `Compiled` pin fixture is
/// then exported and distilled as a one-fixture sheet); for an `authored`/`declined` fixture nothing in
/// the `.fix` (the vault log keeps TradingView's text). No matching reading → `Err`, escalated (§9).
///
/// `pin:06-paste-single-reading` — a pin fixture with two readings of the pasted kind is `Err`
/// (the paste cannot say which).
pub fn record_paste(
    fix: &Path,
    outcome: Paste,
    stamp: &CaptureStamp,
    verify: bool,
) -> Result<DistilReport, String> {
    check_stamp(stamp)?;
    let tracked = load(fix)?;
    let mut f = tracked.clone();
    let mut rep = DistilReport::default();
    if !f.oracle.is_captured_kind() {
        return Ok(rep);
    }
    let body = if f.id.group() == Group::Z {
        match outcome {
            Paste::Compiled => Expect::Parse { accept: true },
            Paste::Refused => Expect::Parse { accept: false },
            Paste::RuntimeError => {
                return Err(format!(
                    "{}: a runtime error on a parse probe (§9)",
                    f.id.as_str()
                ))
            }
        }
    } else {
        let want = |b: &ReadingBody| match outcome {
            Paste::Refused => matches!(b, ReadingBody::Refuse(_)),
            Paste::RuntimeError => matches!(b, ReadingBody::Fault { .. }),
            Paste::Compiled => false,
        };
        if outcome == Paste::Compiled {
            // Exported and distilled next, as a one-fixture sheet.
            return Ok(rep);
        }
        let hits: Vec<&ReadingBody> = f
            .readings
            .iter()
            .map(|r| &r.body)
            .filter(|b| want(b))
            .collect();
        match hits.as_slice() {
            [ReadingBody::Refuse(code)] => Expect::Refuse { code: *code },
            [ReadingBody::Fault { kind, row }] => Expect::Fault {
                kind: *kind,
                row: *row,
            },
            [] => {
                return Err(format!(
                    "{}: no reading predicts {outcome:?} — record and escalate (§9)",
                    f.id.as_str()
                ))
            }
            _ => {
                return Err(format!(
                    "{}: more than one reading predicts {outcome:?} (pin:06-paste-single-reading)",
                    f.id.as_str()
                ))
            }
        }
    };
    if !verify {
        if let Some(why) = i9_refusal(&f, &body, stamp) {
            rep.refused.push((f.id, why));
            return Ok(rep);
        }
    }
    apply(&mut f, body, stamp, None)?;
    finish(fix, &f, &tracked, verify, &mut rep)?;
    Ok(rep)
}
