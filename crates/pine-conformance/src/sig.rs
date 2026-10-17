// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Divergence signatures (impl-06 §3.9, §4.6.3) — what turns 40 red fixtures into 3 causes.
//! Compare-time code, after the loop: allocation is free here.
//!
//! `pin:06-sig-order` — §3.9 returns one `Kind`: the first matching row of §4.6.3's table, top to
//! bottom (`NaMask` is "reported first"). `DialectDivergence` is the verdict's own (triage sets it);
//! `OffByOneFill` / `TradeTimeShift` match a trade list ([`match_trade_signature`]).

use crate::cmp::{compare_cell, ulps_between};
use crate::fixture::{Cell, Fixture, ReadingBody, Row, TradeRow};
use crate::run::RunOutcome;

/// A failure shape (§4.6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The `isna` plot (or the na mask of the plot itself) diverges.
    NaMask,
    /// `a[i] == e[i±1]` on ≥ 90 % of the window.
    OffByOneWarmup,
    /// `ulps(e[i], a[i])` decreases monotonically and is `< 4` on the last 10 %.
    SeedMismatch,
    /// `a[i] − e[i]` constant within 4 ULP on ≥ 90 %.
    ConstantOffset,
    /// `a/e` within 4 ULP of a D18 unit factor.
    Scale1e6,
    /// `a[i] == −e[i]` on ≥ 90 %.
    SignFlip,
    /// `|a − e|` grows monotonically with relative error `< 1e−9`.
    DriftLinear,
    /// The actual series equals one of the fixture's readings.
    PinReading,
    /// Our v5 and v6 outcomes of a `56` fixture differ.
    DialectDivergence,
    /// Group E: every entry price equals the previous or next bar's open.
    OffByOneFill,
    /// Group E: every trade time is shifted by one bar.
    TradeTimeShift,
}

/// The `Kind`'s name as the report prints it.
pub const fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::NaMask => "NaMask",
        Kind::OffByOneWarmup => "OffByOneWarmup",
        Kind::SeedMismatch => "SeedMismatch",
        Kind::ConstantOffset => "ConstantOffset",
        Kind::Scale1e6 => "Scale1e6",
        Kind::SignFlip => "SignFlip",
        Kind::DriftLinear => "DriftLinear",
        Kind::PinReading => "PinReading",
        Kind::DialectDivergence => "DialectDivergence",
        Kind::OffByOneFill => "OffByOneFill",
        Kind::TradeTimeShift => "TradeTimeShift",
    }
}

/// §4.6.3's unit factors for `Scale1e6`.
const SCALES: [f64; 6] = [1e6, 1e-6, 1e12, 1e-12, 100.0, 0.01];

/// The actual value of plot `p` at row key `key`, when the run's window holds the row.
pub fn actual_at(o: &RunOutcome, key: i64, p: usize) -> Option<f64> {
    let np = o.plot_ix.len();
    let r = o.keys.binary_search(&key).ok()?;
    o.plots.get(r * np + p).copied()
}

/// `(expected, actual)` of plot `p` over the expected rows the run's window holds; `None` cells are
/// `na`.
fn pairs(expected: &[Row], o: &RunOutcome, p: usize) -> Vec<(Option<f64>, f64)> {
    let mut out = Vec::with_capacity(expected.len());
    for row in expected {
        let Some(a) = actual_at(o, row.key, p) else {
            continue;
        };
        let e = match row.cells.get(p) {
            Some(Cell::Val { v, .. }) => Some(*v),
            _ => None,
        };
        out.push((e, a));
    }
    out
}

/// `n` of `m` is at least 90 %.
fn at_least_90(n: usize, m: usize) -> bool {
    m > 0 && n * 10 >= m * 9
}

fn isna_plot(f: &Fixture) -> Option<usize> {
    f.plots.iter().position(|p| p.title.ends_with("_isna"))
}

fn na_mask_diverges(expected: &[Row], o: &RunOutcome, p: usize) -> bool {
    pairs(expected, o, p)
        .iter()
        .any(|(e, a)| e.is_none() != a.is_nan())
}

/// The name of the first reading whose rows the run reproduces exactly (under each plot's
/// tolerance), on every row and plot the reading lists.
pub fn matching_reading<'f>(f: &'f Fixture, o: &RunOutcome) -> Option<&'f str> {
    'next: for r in &f.readings {
        let ReadingBody::Rows(rows) = &r.body else {
            continue;
        };
        if rows.is_empty() {
            continue;
        }
        for row in rows {
            for (p, cell) in row.cells.iter().enumerate() {
                let Some(a) = actual_at(o, row.key, p) else {
                    continue 'next;
                };
                let tol = f.plots.get(p).map_or(crate::fixture::Tol::Exact, |s| s.tol);
                if !compare_cell(cell, a, tol).ok() {
                    continue 'next;
                }
            }
        }
        return Some(&r.name);
    }
    None
}

/// Runs every matcher of §4.6.3 over one divergent plot (`plot` indexes `Fixture::plots`).
pub fn match_signature(
    f: &Fixture,
    expected: &[Row],
    actual: &RunOutcome,
    plot: usize,
) -> Option<Kind> {
    if let Some(ip) = isna_plot(f) {
        if na_mask_diverges(expected, actual, ip) || ip_values_diverge(expected, actual, ip) {
            return Some(Kind::NaMask);
        }
    }
    if na_mask_diverges(expected, actual, plot) {
        return Some(Kind::NaMask);
    }
    // Values where both sides are numbers, in window order.
    let v: Vec<(f64, f64)> = pairs(expected, actual, plot)
        .into_iter()
        .filter_map(|(e, a)| e.map(|e| (e, a)))
        .collect();
    let m = v.len();
    if m > 1 && off_by_one(&v) {
        return Some(Kind::OffByOneWarmup);
    }
    if m > 1 && seed_mismatch(&v) {
        return Some(Kind::SeedMismatch);
    }
    if m > 0 && constant_offset(&v) {
        return Some(Kind::ConstantOffset);
    }
    if m > 0 && scale(&v) {
        return Some(Kind::Scale1e6);
    }
    if m > 0 && at_least_90(v.iter().filter(|(e, a)| *a == -*e).count(), m) {
        return Some(Kind::SignFlip);
    }
    if m > 1 && drift_linear(&v) {
        return Some(Kind::DriftLinear);
    }
    if matching_reading(f, actual).is_some() {
        return Some(Kind::PinReading);
    }
    None
}

fn ip_values_diverge(expected: &[Row], o: &RunOutcome, ip: usize) -> bool {
    pairs(expected, o, ip).iter().any(
        |(e, a)| matches!(e, Some(e) if e.to_bits() != a.to_bits() && !(*e == 0.0 && *a == 0.0)),
    )
}

/// `pin:06-sig-off-by-one` — "`a[i] == e[i±1]` on ≥ 90 % of the window": one direction for the
/// whole window (a shift is one shift), over the `m − 1` comparable positions.
fn off_by_one(v: &[(f64, f64)]) -> bool {
    let m = v.len();
    let (mut lag, mut lead) = (0usize, 0usize);
    let mut i = 1usize;
    while i < m {
        if v[i].1 == v[i - 1].0 {
            lag += 1;
        }
        if v[i - 1].1 == v[i].0 {
            lead += 1;
        }
        i += 1;
    }
    at_least_90(lag.max(lead), m - 1)
}

/// `pin:06-sig-seed` — "decreases monotonically" is non-increasing over the window with the first
/// above the last; "the last 10 %" is the last `⌈m/10⌉` rows.
fn seed_mismatch(v: &[(f64, f64)]) -> bool {
    let u: Vec<u64> = v.iter().map(|(e, a)| ulps_between(*e, *a)).collect();
    let m = u.len();
    let tail = m.div_ceil(10);
    u.windows(2).all(|w| w[1] <= w[0]) && u[0] > u[m - 1] && u[m - tail..].iter().all(|x| *x < 4)
}

/// `pin:06-sig-offset-ref` — "constant within 4 ULP": every difference is compared with the median
/// difference, which must be non-zero.
fn constant_offset(v: &[(f64, f64)]) -> bool {
    let mut d: Vec<f64> = v.iter().map(|(e, a)| a - e).collect();
    d.sort_by(f64::total_cmp);
    let med = d[d.len() / 2];
    med != 0.0
        && at_least_90(
            v.iter()
                .filter(|(e, a)| ulps_between(a - e, med) <= 4)
                .count(),
            v.len(),
        )
}

/// `pin:06-sig-scale-every` — one factor for every pair with `e ≠ 0` (at least one such pair).
fn scale(v: &[(f64, f64)]) -> bool {
    let nz: Vec<f64> = v
        .iter()
        .filter(|(e, _)| *e != 0.0)
        .map(|(e, a)| a / e)
        .collect();
    !nz.is_empty()
        && SCALES
            .iter()
            .any(|s| nz.iter().all(|r| ulps_between(*r, *s) <= 4))
}

/// `pin:06-sig-drift` — "grows monotonically": non-decreasing with the last above the first; the
/// relative error `|a − e| / |e| < 1e−9` on every pair with `e ≠ 0`.
fn drift_linear(v: &[(f64, f64)]) -> bool {
    let d: Vec<f64> = v.iter().map(|(e, a)| (a - e).abs()).collect();
    let m = d.len();
    d.windows(2).all(|w| w[1] >= w[0])
        && d[m - 1] > d[0]
        && v.iter()
            .all(|(e, a)| *e == 0.0 || ((a - e).abs() / e.abs()) < 1e-9)
}

/// Group E's trade-list matchers. `bar_ms` is the chart bar's duration; `opens` the chart's
/// `(open_ms, open_px_1e6)` in time order when the caller has the bars.
///
/// `pin:06-sig-trade-opens` — §3.9's `match_signature` carries no bars: `OffByOneFill` needs the
/// opens, so it matches only when `opens` is given (triage passes `None`).
pub fn match_trade_signature(
    expected: &[TradeRow],
    actual: &[pine_ir::TradeRec],
    bar_ms: i64,
    opens: Option<&[(i64, i64)]>,
) -> Option<Kind> {
    let m = expected.len().min(actual.len());
    if m == 0 {
        return None;
    }
    if let Some(op) = opens {
        let fill = (0..m).all(|i| {
            let a = &actual[i];
            match op.binary_search_by_key(&expected[i].entry_ms, |(t, _)| *t) {
                Ok(j) => {
                    let prev = j.checked_sub(1).map(|k| op[k].1);
                    let next = op.get(j + 1).map(|x| x.1);
                    prev == Some(a.entry_px_1e6) || next == Some(a.entry_px_1e6)
                }
                Err(_) => false,
            }
        });
        if fill {
            return Some(Kind::OffByOneFill);
        }
    }
    let shift = (0..m).all(|i| (actual[i].entry_ms - expected[i].entry_ms).abs() == bar_ms);
    if shift && bar_ms > 0 {
        return Some(Kind::TradeTimeShift);
    }
    None
}
