// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Export in, `.fix` out (impl-06 §3.10, §4.7.5): `distil` (a sheet's chart export, optionally a
//! strategy's trades and summary) and `record_paste` (a fixture pasted alone). `verify = true`
//! compares instead of writing (the re-verification session). Offline capture tooling.

use std::path::{Path, PathBuf};

use pine_ir::Dialect;

use super::csv::ChartExport;
use crate::fixture::{
    is_capture_utc, is_tv_build, Align, Cell, Dialects, Expect, Fixture, FixtureId, Group,
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

/// Rows of the export selected by the fixture's alignment and window: `Step` keys are the export's
/// row index (bar 0 = the first exported bar), `Time` keys the bar open ms.
fn select(f: &Fixture, export: &ChartExport, cols: &[usize]) -> Vec<Row> {
    let n = export.rows.len();
    let (lo, hi) = match f.window {
        Window::Head(k) => (0, (k as usize).min(n)),
        Window::Tail(k) => (n.saturating_sub(k as usize), n),
        Window::Time { .. } => (0, n),
    };
    let mut out = Vec::with_capacity(hi - lo);
    let mut i = lo;
    while i < hi {
        let (t, cells) = &export.rows[i];
        let in_window = match f.window {
            Window::Time { from_ms, to_ms } => *t >= from_ms && *t < to_ms,
            _ => true,
        };
        if in_window {
            let key = match f.align {
                Align::Step => i as i64,
                Align::Time => *t,
            };
            out.push(Row {
                key,
                cells: cols.iter().map(|c| cells[*c].clone()).collect(),
            });
        }
        i += 1;
    }
    out
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

/// Applies a new capture to `f`; returns whether the tracked file changes in its body.
///
/// `pin:06-distil-56` — a `56` fixture's second dialect capture must reproduce the first exactly
/// (I9: TradingView's v5 and v6 agree, or the fixture is mis-tabled): a differing body is `Err`.
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
    let prior = f.captured_in.filter(|c| c.0 & !bit.0 != 0);
    if prior.is_some() && f.expect != body {
        return Err(format!(
            "{}: the v{} capture differs from the other dialect's (I9)",
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
        let rows = select(&f, export, &cols);
        let body = match trades {
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
            retol(&mut f, &rows, export.digits);
        }
        let bars = match f.align {
            Align::Step => Some(export.rows.len() as u32),
            Align::Time => f.bars,
        };
        apply(&mut f, body, stamp, bars)?;
        finish(&path, &f, &tracked, verify, &mut rep)?;
    }
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
    apply(&mut f, body, stamp, None)?;
    finish(fix, &f, &tracked, verify, &mut rep)?;
    Ok(rep)
}
