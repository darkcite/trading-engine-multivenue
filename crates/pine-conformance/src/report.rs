// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! What `Runner::check` concludes about one fixture (impl-06 §3.9). This edition carries the
//! verdict model T15 needs; the §4.6.4 `render` lands with T17 (`triage`, `sig`).

/// One fixture's verdict.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Every assertion held.
    Pass,
    /// `Expect::Pending`: the run completed (smoke); compared to nothing.
    Pending,
    /// Not run (counted, I20).
    Skipped(crate::run::SkipReason),
    /// An assertion failed.
    Fail(Divergence),
    /// Our own v5 and v6 runs of a `56` fixture differ (I9) — before any expectation is read.
    DialectDivergence {
        /// The first differing row key ([`NO_ROW`] when the runs differ in outcome, not in a row).
        first_row: i64,
        /// The first differing plot (`usize::MAX` when no plot is involved).
        plot: usize,
    },
}

/// `pin:06-divergence-no-row` — §3.9's `first_row` is an `i64` with no "none": a failure that is
/// not about a row (a wrong refusal code, an unexpected fault, a run that should have been refused)
/// carries `i64::MIN`.
pub const NO_ROW: i64 = i64::MIN;

/// Where a fixture's expectation and our run part.
#[derive(Debug, Clone)]
pub struct Divergence {
    /// The first divergent plot (`Fixture::plots` index).
    pub plot: Option<usize>,
    /// The first divergent row key (inside or outside the tolerance), or [`NO_ROW`].
    pub first_row: i64,
    /// The first row key outside the tolerance.
    pub first_out_of_bound: Option<i64>,
    /// The trade-list or summary divergence.
    pub trade: Option<crate::trades::TradeDiv>,
    /// A run error or an expectation the run could not meet, as text.
    pub error: Option<String>,
}

impl Divergence {
    /// A divergence that is only an error text.
    pub fn error(what: String) -> Self {
        Self {
            plot: None,
            first_row: NO_ROW,
            first_out_of_bound: None,
            trade: None,
            error: Some(what),
        }
    }
}

// --------------------------------------------------------------------------------------------
// §4.6.4 — the failure report (T17). Compare-time code: allocation is free here.
// --------------------------------------------------------------------------------------------

use crate::cmp::{compare_cell_fmt, format_sig, Cmp};
use crate::fixture::{Cell, Dialects, Expect, Fixture, Group, Row, Stamp, Tol};
use crate::run::RunOutcome;
use crate::sig::{actual_at, kind_name, matching_reading, Kind};
use crate::triage::{bucket_name, owner_name, Bucket, Triage};

fn hex_prefix(h: &[u8; 32], bytes: usize) -> String {
    let mut s = String::with_capacity(bytes * 2 + 3);
    for b in &h[..bytes] {
        s.push_str(&format!("{b:02x}"));
    }
    s.push('…');
    s
}

fn stamp(s: &Stamp) -> &str {
    match s {
        Stamp::Pending => "pending",
        Stamp::NotApplicable => "n/a",
        Stamp::At(t) => t,
    }
}

fn dialects(d: Dialects) -> &'static str {
    match d.0 {
        0b01 => "5",
        0b10 => "6",
        0b11 => "56",
        _ => "-",
    }
}

fn tol(t: Tol) -> String {
    match t {
        Tol::Exact => "exact".into(),
        Tol::Ulp(n) => format!("ulp {n}"),
        Tol::Dec(s) => format!("dec {s}"),
    }
}

/// `pin:06-report-ulp-notation` — the ULP count prints as an integer below 10⁶, else as
/// `d.dde+NN` (the §4.6.4 sample's `1.13e+13`).
fn ulp_text(u: u64) -> String {
    if u < 1_000_000 {
        return format!("{u} ulp");
    }
    let e = format!("{:.2e}", u as f64);
    match e.split_once('e') {
        Some((m, x)) if !x.starts_with('-') => format!("{m}e+{x} ulp"),
        _ => format!("{e} ulp"),
    }
}

fn expected_rows(f: &Fixture) -> &[Row] {
    match &f.expect {
        Expect::Series(rows) => rows,
        Expect::Strategy { series, .. } => series,
        _ => &[],
    }
}

fn cell_text(c: &Cell) -> &str {
    match c {
        Cell::Na => "na",
        Cell::Val { text, .. } => text,
    }
}

fn value_text(a: f64) -> String {
    if a.is_nan() {
        "na".into()
    } else if a.is_infinite() {
        format_sig(a, 17)
    } else {
        format!("{a:?}")
    }
}

/// One `expected` / `actual` pair at row `key` of plot `p`.
fn pair_lines(
    out: &mut String,
    f: &Fixture,
    p: usize,
    key: i64,
    o: Option<&RunOutcome>,
    bound: &str,
) {
    let t = f.plots.get(p).map_or(Tol::Exact, |s| s.tol);
    let cell = expected_rows(f)
        .iter()
        .find(|r| r.key == key)
        .and_then(|r| r.cells.get(p));
    let exp = cell.map_or("(no expected row)", cell_text);
    out.push_str(&format!("      expected   {exp}\n"));
    match (o.and_then(|o| actual_at(o, key, p)), cell) {
        (Some(a), Some(c)) => {
            let delta = match (compare_cell_fmt(c, a, t, false), t, c) {
                (Cmp::Equal, ..) => "0 ulp".to_string(),
                (_, Tol::Dec(_), Cell::Val { v, .. }) => format!("Δ {:.3e}", (a - v).abs()),
                (Cmp::Divergent { ulps, .. }, ..) => ulp_text(ulps),
            };
            let within = if compare_cell_fmt(c, a, t, false).ok() {
                "WITHIN BOUND"
            } else {
                "OUT OF BOUND"
            };
            out.push_str(&format!(
                "      actual     {:<24} {delta:<14} {within}\n",
                value_text(a)
            ));
        }
        _ if bound.is_empty() => out.push_str("      actual     (not retained)\n"),
        _ => out.push_str(&format!("      actual     (not retained)  {bound}\n")),
    }
}

/// `pin:06-report-span-offset` — §3.9's `render` has no source text, so the plot's location is
/// `<script file>@<byte offset>` of `ProgramMeta::plots[i].span` (matched by title through the
/// program's `SymTable`); `-` without a program or a matching plot.
fn plot_loc(f: &Fixture, p: usize, prog: Option<&pine_ir::PineProgram>) -> String {
    let file = f.script.rsplit('/').next().unwrap_or(&f.script);
    let Some(title) = f.plots.get(p).map(|s| s.title.as_str()) else {
        return "-".into();
    };
    prog.and_then(|g| {
        g.meta
            .plots
            .iter()
            .find(|d| g.syms.resolve(d.title) == title)
            .map(|d| format!("{file}@{} (PlotDesc.span)", d.span.lo))
    })
    .unwrap_or_else(|| "-".into())
}

/// The §4.6.4 text, byte-stable (golden-tested).
pub fn render(f: &Fixture, v: &Verdict, t: &Triage, prog: Option<&pine_ir::PineProgram>) -> String {
    render_with(f, v, t, prog, None)
}

/// [`render`] with the run's outcome, so the report can print the actual values and the bound.
///
/// `pin:06-report-actual-from-outcome` — §3.9's `render` takes no `RunOutcome`, yet §4.6.4 requires
/// "expected vs actual": without the outcome the actual line reads `(not retained)`.
pub fn render_with(
    f: &Fixture,
    v: &Verdict,
    t: &Triage,
    prog: Option<&pine_ir::PineProgram>,
    o: Option<&RunOutcome>,
) -> String {
    let id = f.id.as_str();
    let g = f.id.group().letter();
    let head = match v {
        Verdict::Pass => "PASS",
        Verdict::Pending => "PEND",
        Verdict::Skipped(_) => "SKIP",
        Verdict::Fail(_) | Verdict::DialectDivergence { .. } => "FAIL",
    };
    let mut out = String::new();
    out.push_str(&format!(
        "{head}  {id}  {:<44}  {}\n",
        f.title,
        f.pins.join(" ")
    ));
    out.push_str(&format!(
        "  fixture : tests/conformance/fixtures/{g}/{id}.fix\n"
    ));
    // pin:06-report-script-ok — the run checked I2 (a mismatch is a 7003 error, never a report).
    out.push_str(&format!(
        "  script  : {}    sha {}  OK\n",
        f.script,
        hex_prefix(&f.script_sha256, 3)
    ));
    out.push_str(&format!(
        "  oracle  : {}   captured {}   tv_build {}   captured_in {}\n",
        crate::fixture::oracle_text(f.oracle),
        stamp(&f.captured),
        stamp(&f.tv_build),
        f.captured_in.map_or("-", dialects)
    ));
    let chart_ok = if t.bucket == Bucket::ChartDrift {
        "DRIFT"
    } else {
        "OK"
    };
    let dia = match (f.dialects == Dialects::BOTH, v) {
        (true, Verdict::DialectDivergence { .. }) => "v5 ≢ v6".to_string(),
        (true, _) => "v5 ≡ v6".to_string(),
        (false, _) => format!("v{}", dialects(f.dialects)),
    };
    out.push_str(&format!(
        "  chart   : {}  {chart_ok}        align {}, bars {}        dialect: {dia}\n",
        hex_prefix(&f.chart_sha256, 2),
        crate::fixture::align_name(f.align),
        f.bars.map_or("-".to_string(), |b| b.to_string()),
    ));
    match v {
        Verdict::Fail(d) => {
            if let Some(p) = d.plot {
                let title = f.plots.get(p).map_or("?", |s| s.title.as_str());
                let tl = f.plots.get(p).map_or(Tol::Exact, |s| s.tol);
                out.push_str(&format!(
                    "  plot {p}  \"{title}\"  {}  tolerance {}\n",
                    plot_loc(f, p, prog),
                    tol(tl)
                ));
                if d.first_row != NO_ROW {
                    out.push_str(&format!("  first divergent row : {}\n", d.first_row));
                    pair_lines(&mut out, f, p, d.first_row, o, "");
                }
                if let Some(k) = d.first_out_of_bound {
                    out.push_str(&format!("  first OUT-OF-BOUND row : {k}\n"));
                    pair_lines(&mut out, f, p, k, o, "OUT OF BOUND");
                }
                if let Some(ip) = f.plots.iter().position(|s| s.title.ends_with("_isna")) {
                    if ip != p {
                        let n = match o {
                            Some(o) => expected_rows(f)
                                .iter()
                                .filter(|r| {
                                    let a = actual_at(o, r.key, ip);
                                    match (r.cells.get(ip), a) {
                                        (Some(c), Some(a)) => {
                                            !compare_cell_fmt(c, a, Tol::Exact, false).ok()
                                        }
                                        _ => true,
                                    }
                                })
                                .count()
                                .to_string(),
                            None => "(not retained)".into(),
                        };
                        out.push_str(&format!(
                            "  plot {ip}  \"{}\" exact : {n} divergences\n",
                            f.plots[ip].title
                        ));
                    }
                }
            }
            if let Some(tr) = &d.trade {
                out.push_str(&format!("  trades  : {tr:?}\n"));
            }
            if let Some(e) = &d.error {
                out.push_str(&format!("  error   : {e}\n"));
            }
        }
        Verdict::DialectDivergence { first_row, plot } => {
            out.push_str(&format!(
                "  dialect divergence : row {first_row}, plot {plot}\n"
            ));
        }
        Verdict::Skipped(s) => out.push_str(&format!("  skipped : {s:?}\n")),
        Verdict::Pass | Verdict::Pending => {}
    }
    let sig = match (t.signature, o) {
        (Some(Kind::PinReading), Some(o)) => match matching_reading(f, o) {
            Some(r) => format!("PinReading (actual = reading {r})"),
            None => "PinReading".into(),
        },
        (Some(k), _) => kind_name(k).to_string(),
        (None, _) => "none".into(),
    };
    out.push_str(&format!("  signature : {sig}\n"));
    let pin_owned = !f.pins.is_empty() && !f.readings.is_empty();
    out.push_str(&format!(
        "  bucket    : {:<16} owner: {}{}\n",
        bucket_name(t.bucket),
        owner_name(t.owner),
        if pin_owned { " (the pin's switch)" } else { "" }
    ));
    // pin:06-report-note — the note line is printed for a pin-owned failure and a Declined one.
    if pin_owned && t.bucket == Bucket::SemanticBug {
        out.push_str(&format!(
            "  note      : decide {} first (pine-conf pins --decide), then flip its switch.\n",
            f.pins[0]
        ));
    } else if t.bucket == Bucket::Declined {
        out.push_str("  note      : reclassify in the generator table.\n");
    }
    out
}

// --------------------------------------------------------------------------------------------
// The TradingView divergence report (R-P1, `pine-conf report --tv`). Compare-time code.
// --------------------------------------------------------------------------------------------

use crate::triage::{crate_name, tv_class_name, Crate, TvClass, TvTriage};

const TV_CLASSES: [TvClass; 5] = [
    TvClass::Pass,
    TvClass::Differ,
    TvClass::RefusedByUs,
    TvClass::FaultByUs,
    TvClass::HarnessGap,
];

/// `(pass, differ, refused-by-us, fault-by-us, harness-gap)` over `rows`.
pub fn tv_counts(rows: &[(&Fixture, TvTriage)]) -> [usize; 5] {
    let mut c = [0usize; 5];
    for (_, t) in rows {
        c[TV_CLASSES
            .iter()
            .position(|x| *x == t.class)
            .expect("five classes")] += 1;
    }
    c
}

/// The clusters of the non-pass rows, largest first: `(cluster, crate, ids)`.
pub fn tv_clusters<'a>(rows: &'a [(&'a Fixture, TvTriage)]) -> Vec<(String, Crate, Vec<&'a str>)> {
    let mut out: Vec<(String, Crate, Vec<&str>)> = Vec::new();
    for (f, t) in rows.iter().filter(|(_, t)| t.class != TvClass::Pass) {
        match out.iter_mut().find(|(k, _, _)| *k == t.cluster) {
            Some((_, _, ids)) => ids.push(f.id.as_str()),
            None => out.push((t.cluster.clone(), t.krate, vec![f.id.as_str()])),
        }
    }
    out.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then_with(|| a.0.cmp(&b.0)));
    out
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|")
}

fn ids_text(ids: &[&str], max: usize) -> String {
    let mut s = ids.iter().take(max).copied().collect::<Vec<_>>().join(" ");
    if ids.len() > max {
        s.push_str(&format!(" … (+{})", ids.len() - max));
    }
    s
}

/// The divergence report (Markdown): totals per group, the clusters, every non-pass by crate and
/// by pin with its first difference, the pass list. `context` lines print under the title.
pub fn render_tv(title: &str, context: &[String], rows: &[(&Fixture, TvTriage)]) -> String {
    let mut o = String::new();
    o.push_str(&format!("# {title}\n\n"));
    for l in context {
        o.push_str(&format!("- {l}\n"));
    }
    let c = tv_counts(rows);
    let near: Vec<&(&Fixture, TvTriage)> = rows
        .iter()
        .filter(|(_, t)| {
            t.class == TvClass::Pass
                && t.note
                    .as_deref()
                    .is_some_and(|n| n.contains(crate::triage::NEAR_NOTE_PREFIX))
        })
        .collect();
    o.push_str(&format!(
        "\n**{} fixtures: pass {} · differ {} · refused-by-us {} · fault-by-us {} · harness-gap {}**\n",
        rows.len(),
        c[0],
        c[1],
        c[2],
        c[3],
        c[4]
    ));
    if !near.is_empty() {
        o.push_str(&format!(
            "\nOf the passes, {} rest on the 2-decimal fallback (R-P34: a trade-list money cell within \
             0.01 of TradingView's printed cell, not within half a cent) — listed in \
             \"Pass on the 2-decimal fallback\".\n",
            near.len()
        ));
    }
    o.push_str("\n## Totals per group\n\n| group | ran | pass | differ | refused-by-us | fault-by-us | harness-gap |\n|---|---|---|---|---|---|---|\n");
    for g in [
        Group::A,
        Group::B,
        Group::C,
        Group::D,
        Group::E,
        Group::F,
        Group::Z,
    ] {
        let sub: Vec<(&Fixture, TvTriage)> = rows
            .iter()
            .filter(|(f, _)| f.id.group() == g)
            .map(|(f, t)| (*f, t.clone()))
            .collect();
        if sub.is_empty() {
            continue;
        }
        let k = tv_counts(&sub);
        o.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            g.letter(),
            sub.len(),
            k[0],
            k[1],
            k[2],
            k[3],
            k[4]
        ));
    }
    o.push_str(&format!(
        "| **all** | {} | {} | {} | {} | {} | {} |\n",
        rows.len(),
        c[0],
        c[1],
        c[2],
        c[3],
        c[4]
    ));
    // Per crate totals.
    o.push_str("\n## Non-pass per probable crate\n\n| crate | differ | refused-by-us | fault-by-us | harness-gap |\n|---|---|---|---|---|\n");
    for k in [
        Crate::Lang,
        Crate::Sema,
        Crate::Std,
        Crate::Vm,
        Crate::Broker,
        Crate::Harness,
    ] {
        let n = |cl: TvClass| {
            rows.iter()
                .filter(|(_, t)| t.krate == k && t.class == cl)
                .count()
        };
        let row = [
            n(TvClass::Differ),
            n(TvClass::RefusedByUs),
            n(TvClass::FaultByUs),
            n(TvClass::HarnessGap),
        ];
        if row.iter().sum::<usize>() > 0 {
            o.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                crate_name(k),
                row[0],
                row[1],
                row[2],
                row[3]
            ));
        }
    }
    let clusters = tv_clusters(rows);
    o.push_str("\n## Clusters (largest first)\n\n| # | n | crate | cluster | fixtures |\n|---|---|---|---|---|\n");
    for (i, (k, cr, ids)) in clusters.iter().enumerate() {
        o.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            i + 1,
            ids.len(),
            crate_name(*cr),
            md_cell(k),
            ids_text(ids, 12)
        ));
    }
    o.push_str("\n## Every non-pass by crate (first difference)\n");
    for k in [
        Crate::Lang,
        Crate::Sema,
        Crate::Std,
        Crate::Vm,
        Crate::Broker,
        Crate::Harness,
    ] {
        let sub: Vec<&(&Fixture, TvTriage)> = rows
            .iter()
            .filter(|(_, t)| t.class != TvClass::Pass && t.krate == k)
            .collect();
        if sub.is_empty() {
            continue;
        }
        o.push_str(&format!(
            "\n### {} ({})\n\n| id | class | pins | first difference | note |\n|---|---|---|---|---|\n",
            crate_name(k),
            sub.len()
        ));
        for (f, t) in sub {
            o.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                f.id.as_str(),
                tv_class_name(t.class),
                md_cell(&f.pins.join(" ")),
                md_cell(&t.first),
                md_cell(t.note.as_deref().unwrap_or(""))
            ));
        }
    }
    // By pin.
    let mut pins: Vec<&str> = rows
        .iter()
        .filter(|(_, t)| t.class != TvClass::Pass)
        .flat_map(|(f, _)| f.pins.iter().map(String::as_str))
        .collect();
    pins.sort_unstable();
    pins.dedup();
    o.push_str("\n## Every non-pass by pin\n\n| pin | fixtures (class, crate) |\n|---|---|\n");
    for p in pins {
        let list: Vec<String> = rows
            .iter()
            .filter(|(f, t)| t.class != TvClass::Pass && f.pins.iter().any(|x| x == p))
            .map(|(f, t)| {
                format!(
                    "{} ({}, {})",
                    f.id.as_str(),
                    tv_class_name(t.class),
                    crate_name(t.krate)
                )
            })
            .collect();
        o.push_str(&format!("| {} | {} |\n", md_cell(p), list.join(" · ")));
    }
    let unpinned: Vec<&str> = rows
        .iter()
        .filter(|(f, t)| t.class != TvClass::Pass && f.pins.is_empty())
        .map(|(f, _)| f.id.as_str())
        .collect();
    o.push_str(&format!(
        "| (no pin) | {} fixtures (see the crate tables) |\n",
        unpinned.len()
    ));
    let pass: Vec<&str> = rows
        .iter()
        .filter(|(_, t)| t.class == TvClass::Pass)
        .map(|(f, _)| f.id.as_str())
        .collect();
    o.push_str(&format!(
        "\n## Pass ({})\n\n{}\n",
        pass.len(),
        pass.join(" ")
    ));
    if !near.is_empty() {
        o.push_str(&format!(
            "\n## Pass on the 2-decimal fallback ({}, R-P34)\n\n| id | pins | cells |\n|---|---|---|\n",
            near.len()
        ));
        for (f, t) in &near {
            o.push_str(&format!(
                "| {} | {} | {} |\n",
                f.id.as_str(),
                md_cell(&f.pins.join(" ")),
                md_cell(t.note.as_deref().unwrap_or(""))
            ));
        }
    }
    let noted: Vec<String> = rows
        .iter()
        .filter(|(_, t)| t.class == TvClass::Pass && t.note.is_some())
        .map(|(f, t)| format!("{}: {}", f.id.as_str(), t.note.as_deref().unwrap_or("")))
        .collect();
    if !noted.is_empty() {
        o.push_str("\nPass notes:\n\n");
        for n in noted {
            o.push_str(&format!("- {n}\n"));
        }
    }
    o
}
