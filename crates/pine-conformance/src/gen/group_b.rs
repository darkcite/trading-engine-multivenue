// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Group B — `ta.*` warm-up, window edges, recursion (impl-06 §4.2.2). GENERATOR-TIME ONLY.
//!
//! B-core (B001–B060, `B(6·f + e + 1)`), B-pin (B061–B076), then the data-derived blocks: B-wide
//! (every remaining satisfiable tier-1 `ta.*` of `pine_spec`, by name) and B-chart (every
//! [`IMPLICIT_BAR_READERS`] name). B-wide and B-chart carry no `pin` line (I15).
//!
//! Row notation as group F: `@` stands for `_<ID>`; each leading `>` is one block level.

use pine_ir::{Dialect, Qual, Ty};
use pine_spec::{params, pf, rets, Tier};

use super::emit::{Line, Prelude, Script};
use super::readings::{
    ema, monotone, pair, rising, row as arow, sma, sma_last_n_valid, sma_mean_of_valid, EmaSeed, NA,
};
use super::{Cell, Run};
use crate::fixture::{
    Align, Dialects, Driver, Expect, FixtureId, Group, Oracle, Reading, ReadingBody, Tol, Window,
};
use crate::syn::SynSeq;

/// The ten B-core functions, `f` order (§4.2.2).
pub const CORE: [&str; 10] = [
    "ta.sma",
    "ta.ema",
    "ta.rma",
    "ta.wma",
    "ta.stdev",
    "ta.highest",
    "ta.change",
    "ta.macd",
    "ta.rsi",
    "ta.barssince",
];

/// The six B-core edges, `e` order.
pub const EDGES: [&str; 6] = [
    "warmup",
    "len1",
    "len_gt_bars",
    "na_mid",
    "conditional",
    "two_sites",
];

/// Every `ta.*` name B001–B076 already call: B-wide is "every *remaining* tier-1 `ta.*`".
pub const FIXED_NAMES: [&str; 19] = [
    "ta.alma",
    "ta.barssince",
    "ta.change",
    "ta.ema",
    "ta.falling",
    "ta.highest",
    "ta.kc",
    "ta.linreg",
    "ta.macd",
    "ta.mom",
    "ta.percentrank",
    "ta.rising",
    "ta.rma",
    "ta.roc",
    "ta.rsi",
    "ta.sma",
    "ta.stdev",
    "ta.swma",
    "ta.wma",
];

/// The hand list of `ta.*` names that read OHLCV without a `source` argument (§4.2.2), with the
/// B-chart call, its result count (0 = an `array<float>`) and whether it is cumulative (plotted as
/// its one-bar change, §4.1.3). `implicit_bar_readers_exist_in_spec` checks every name.
///
/// `pin:06-b-chart-args` — the arguments are the reference manual's example arguments where it
/// gives them, else `14` for a length and `close` / `hlc3` for a source. `pin:06-b-chart-cumulative`
/// — cumulative = `accdist`, `nvi`, `obv`, `pvi`, `pvt`, `wad` (running sums of a bar quantity).
pub const IMPLICIT_BAR_READERS: [(&str, &str, u8, bool); 21] = [
    ("ta.accdist", "ta.accdist", 1, true),
    ("ta.atr", "ta.atr(14)", 1, false),
    ("ta.dmi", "ta.dmi(14, 14)", 3, false),
    ("ta.iii", "ta.iii", 1, false),
    ("ta.kcw", "ta.kcw(close, 20, 2)", 1, false),
    ("ta.mfi", "ta.mfi(hlc3, 14)", 1, false),
    ("ta.nvi", "ta.nvi", 1, true),
    ("ta.obv", "ta.obv", 1, true),
    (
        "ta.pivot_point_levels",
        "ta.pivot_point_levels(\"Traditional\", timeframe.change(\"D\"))",
        0,
        false,
    ),
    ("ta.pivothigh", "ta.pivothigh(2, 2)", 1, false),
    ("ta.pivotlow", "ta.pivotlow(2, 2)", 1, false),
    ("ta.pvi", "ta.pvi", 1, true),
    ("ta.pvt", "ta.pvt", 1, true),
    ("ta.sar", "ta.sar(0.02, 0.02, 0.2)", 1, false),
    ("ta.supertrend", "ta.supertrend(3, 10)", 2, false),
    ("ta.tr", "ta.tr(true)", 1, false),
    ("ta.vwap", "ta.vwap(hlc3)", 1, false),
    ("ta.vwma", "ta.vwma(close, 14)", 1, false),
    ("ta.wad", "ta.wad", 1, true),
    ("ta.wpr", "ta.wpr(14)", 1, false),
    ("ta.wvad", "ta.wvad", 1, false),
];

const B56: Dialects = Dialects::BOTH;
const REF: &str = "pin:04-ta-reference";

fn line(s: &str, id: FixtureId) -> Line {
    let sfx = format!("_{}", id.as_str());
    let depth = s.bytes().take_while(|&c| c == b'>').count();
    Line {
        depth: depth as u8,
        text: s[depth..].replace('@', &sfx),
    }
}

/// One group-B row before emission.
struct Row {
    n: u16,
    ds: Dialects,
    twin: u16,
    title: String,
    about: Vec<String>,
    pins: Vec<&'static str>,
    prelude: Prelude,
    body: Vec<String>,
    /// `(expression, suffix, tolerance)`.
    plots: Vec<(String, String, Tol)>,
    window: Window,
    tier: u8,
    readings: Vec<Reading>,
    run: Run,
}

fn row(n: u16, title: String, body: Vec<String>, plots: Vec<(String, String, Tol)>) -> Row {
    Row {
        n,
        ds: B56,
        twin: 0,
        about: vec![title.clone()],
        title,
        pins: Vec::new(),
        prelude: Prelude::Syn1,
        body,
        plots,
        window: Window::Head(128),
        tier: 1,
        readings: Vec::new(),
        run: Run::default(),
    }
}

/// The isna/val pair with the value's tolerance (`isna` is always `exact`).
fn iv(e: &str, k: &str, t: Tol) -> Vec<(String, String, Tol)> {
    vec![
        (format!("na({e}) ? 1 : 0"), format!("isna{k}"), Tol::Exact),
        (format!("na({e}) ? na : {e}"), format!("val{k}"), t),
    ]
}

/// §4.2.2's tolerance for `f` at length `len`.
fn tol(f: usize, len: u32) -> Tol {
    match f {
        0 | 3 => Tol::Ulp((4 * len).min(64)),
        4 => Tol::Ulp((4 * len).min(64) + 2),
        1 => Tol::Ulp(len.div_ceil(2) + 2),
        2 => Tol::Ulp(len + 2),
        7 => Tol::Ulp(32),
        8 => Tol::Ulp(2 * len + 6),
        _ => Tol::Exact,
    }
}

/// The value expression of `f` over `src` at `len`; macd binds its tuple first (`pre`).
///
/// `pin:06-b-macd-lengths` — `ta.macd`'s `len1` / `len_gt_bars` edges set all three lengths to the
/// edge's `len`; its other edges use `(12, 26, 9)`; its value is the histogram (§4.2.2).
/// `pin:06-b-barssince-conds` — `ta.barssince`'s `src` is a condition: `_n == 1` / `_n > 200` on
/// the `len` edges and `synna > 1400` for `synna` (§4.2.2); elsewhere `syni % 2 == 0`, chosen
/// independent of `synb` so the conditional edge's two readings differ.
fn call(f: usize, src: &str, len: u32, tag: &str, pre: &mut Vec<String>, depth: &str) -> String {
    match f {
        7 => {
            let l = if len == 1 || len == 200 {
                format!("{len}, {len}, {len}")
            } else {
                "12, 26, 9".to_string()
            };
            pre.push(format!(
                "{depth}[m{tag}@, s{tag}@, h{tag}@] = ta.macd({src}, {l})"
            ));
            format!("h{tag}@")
        }
        9 => {
            let cond = match (src, len) {
                ("synna", _) => "synna > 1400",
                (_, 1) => "_n == 1",
                (_, 200) => "_n > 200",
                _ => "syni % 2 == 0",
            };
            format!("ta.barssince({cond})")
        }
        _ => format!("{}({src}, {len})", CORE[f]),
    }
}

fn core(f: usize, e: usize) -> Row {
    let n = (6 * f + e + 1) as u16;
    let name = CORE[f];
    let mut body = Vec::new();
    let (title, plots, window) = match e {
        0..=3 => {
            let (src, len, title, w) = match e {
                0 => ("syn", 14, format!("{name} warm-up"), Window::Head(128)),
                1 => ("syn", 1, format!("{name} with len 1"), Window::Head(128)),
                2 => (
                    "syn",
                    200,
                    format!("{name} with len 200"),
                    Window::Head(128),
                ),
                _ => ("synna", 14, format!("{name} over synna"), Window::Tail(128)),
            };
            let c = call(f, src, len, "", &mut body, "");
            body.push(format!("v@ = {c}"));
            (title, iv("v@", "", tol(f, len)), w)
        }
        4 => {
            body.push("var float h@ = na".to_string());
            body.push("if synb".to_string());
            let c = call(f, "syn", 14, "h", &mut body, ">");
            body.push(format!(">h@ := {c}"));
            (
                format!("{name} inside a conditional"),
                iv("h@", "", tol(f, 14)),
                Window::Tail(128),
            )
        }
        _ => {
            let c1 = call(f, "syn", 20, "1", &mut body, "");
            body.push(format!("v1@ = {c1}"));
            let c2 = call(f, "synna", 20, "2", &mut body, "");
            body.push(format!("v2@ = {c2}"));
            let mut p = iv("v1@", "1", tol(f, 20));
            p.extend(iv("v2@", "2", tol(f, 20)));
            (format!("{name} at two sites"), p, Window::Tail(128))
        }
    };
    let mut r = row(n, title, body, plots);
    r.about.push(format!("B-core × {}", EDGES[e]));
    r.window = window;
    r.pins.push(REF);
    if n == 4 {
        r.pins.push("pin:W5-sma-na");
    } else if e == 3 {
        r.pins.push("pin:U18-ta-na-propagation");
    }
    if n == 7 {
        r.pins.push("pin:U01-ema-seed");
        r.readings = ema_seed_readings(14, 128);
    }
    if e == 4 {
        r.pins.push("pin:06-cond-call-state");
    }
    r
}

fn syn_n(n: usize) -> Vec<f64> {
    let mut q = SynSeq::new();
    (0..n).map(|_| q.step().syn).collect()
}

fn reading(name: &str, rows: Vec<crate::fixture::Row>) -> Reading {
    Reading {
        name: name.to_string(),
        body: ReadingBody::Rows(rows),
    }
}

fn pair_rows(v: &[f64]) -> Vec<crate::fixture::Row> {
    (0..v.len()).map(|i| arow(i as i64, &pair(v[i]))).collect()
}

/// `pin:U01-ema-seed` readings `first_value` / `sma` of `ta.ema(syn, len)` over `head n`.
fn ema_seed_readings(len: usize, n: usize) -> Vec<Reading> {
    let x = syn_n(n);
    vec![
        reading("first_value", pair_rows(&ema(&x, len, EmaSeed::First))),
        reading("sma", pair_rows(&ema(&x, len, EmaSeed::Sma))),
    ]
}

/// `pin:U03-rising-falling` readings `gt_all_prior` / `monotone` of B064–B067's one plot: v5
/// plots `na` as `−1`; v6 has no `na` bool (W13), so warm-up reads `false`.
fn rising_readings(up: bool, v6: bool) -> Vec<Reading> {
    let x = syn_n(128);
    let gt = if up {
        rising(&x, 3)
    } else {
        super::readings::falling(&x, 3)
    };
    let mono = monotone(&x, 3, up);
    let plot = |v: &[f64]| -> Vec<crate::fixture::Row> {
        (0..v.len())
            .map(|i| {
                let c = if v[i].is_nan() {
                    if v6 {
                        0.0
                    } else {
                        -1.0
                    }
                } else {
                    v[i]
                };
                arow(i as i64, &[c])
            })
            .collect()
    };
    vec![
        reading("gt_all_prior", plot(&gt)),
        reading("monotone", plot(&mono)),
    ]
}

/// `pin:U13-ta-in-loop` readings of B068 (`ta.sma(syn, 5)`) and B069 (`ta.ema(syn + i, 5)`, the
/// U01 default seed) over `head 128`: `per_call` feeds one block the three iterations' inputs in
/// order each step; `per_iteration` keeps one block per iteration index. `acc` sums the three
/// outputs from `0.0`, in iteration order.
fn loop_readings(is_ema: bool) -> Vec<Reading> {
    let x = syn_n(128);
    let f = |s: &[f64]| {
        if is_ema {
            ema(s, 5, EmaSeed::First)
        } else {
            sma(s, 5)
        }
    };
    let input = |i: usize, k: usize| {
        if is_ema {
            x[i] + k as f64
        } else {
            x[i]
        }
    };
    let flat: Vec<f64> = (0..128)
        .flat_map(|i| (0..3).map(move |k| (i, k)))
        .map(|(i, k)| input(i, k))
        .collect();
    let per_call_out = f(&flat);
    let per_iter: Vec<Vec<f64>> = (0..3)
        .map(|k| f(&(0..128).map(|i| input(i, k)).collect::<Vec<_>>()))
        .collect();
    vec![
        reading("per_call", pair_rows(&sum3(|i, k| per_call_out[3 * i + k]))),
        reading("per_iteration", pair_rows(&sum3(|i, k| per_iter[k][i]))),
    ]
}

/// `acc = 0.0; acc += g(i, k)` for `k = 0..3`, per step.
fn sum3(g: impl Fn(usize, usize) -> f64) -> Vec<f64> {
    (0..128)
        .map(|i| {
            let mut acc = 0.0;
            for k in 0..3 {
                acc += g(i, k);
            }
            acc
        })
        .collect()
}

/// `pin:W5-sma-na` readings of B076 (`head 40`, `x` is `na` on row 24).
fn w5_readings() -> Vec<Reading> {
    let mut x = syn_n(40);
    x[24] = NA;
    vec![
        reading("last_n_valid", pair_rows(&sma_last_n_valid(&x, 5))),
        reading("mean_of_valid", pair_rows(&sma_mean_of_valid(&x, 5))),
        reading("propagate", pair_rows(&sma(&x, 5))),
    ]
}

fn chart_run() -> Run {
    Run {
        driver: Driver::Chart,
        align: Align::Time,
        ..Run::default()
    }
}

/// B-pin (B061–B076).
///
/// `pin:06-b-pin-window` — rows that state no window (B064–B075) take B-core's `warmup` window,
/// `head 128`; B070–B075 compare `exact` (§4.2.2 lists no tolerance for them). B062/B063 plot the
/// three `ta.kc` results as written ("3 plots"), Chart, `align time`, `tail 256`, no prelude.
fn pins() -> Vec<Row> {
    let s = |x: &[&str]| x.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    let mut o = Vec::new();
    let mut b61 = row(
        61,
        "ta.ema seed".into(),
        s(&["v@ = ta.ema(syn, 3)"]),
        iv("v@", "", tol(1, 3)),
    );
    b61.window = Window::Head(16);
    b61.pins = vec![REF, "pin:U01-ema-seed"];
    b61.readings = ema_seed_readings(3, 16);
    o.push(b61);
    for (n, args) in [(62u16, "close, 20, 2"), (63, "close, 20, 2, false")] {
        let mut r = row(
            n,
            format!("ta.kc({args})"),
            vec![format!("[m@, u@, l@] = ta.kc({args})")],
            ["m", "u", "l"]
                .iter()
                .map(|k| (format!("{k}@"), k.to_string(), Tol::Exact))
                .collect(),
        );
        r.prelude = Prelude::None;
        r.window = Window::Tail(256);
        r.run = chart_run();
        r.pins = vec![REF, "pin:U02-kc-basis"];
        o.push(r);
    }
    for (n, f, v6) in [
        (64u16, "rising", false),
        (65, "rising", true),
        (66, "falling", false),
        (67, "falling", true),
    ] {
        let p = if v6 {
            "r@ ? 1 : 0"
        } else {
            "na(r@) ? -1 : r@ ? 1 : 0"
        };
        let mut r = row(
            n,
            format!("ta.{f}(syn, 3)"),
            vec![format!("r@ = ta.{f}(syn, 3)")],
            vec![(p.to_string(), "val".to_string(), Tol::Exact)],
        );
        r.ds = if v6 { Dialects::V6 } else { Dialects::V5 };
        r.twin = if v6 { n - 1 } else { n + 1 };
        r.pins = vec![REF, "pin:U03-rising-falling", "pin:W13-bool-na"];
        r.readings = rising_readings(f == "rising", v6);
        o.push(r);
    }
    for (n, k, is_ema) in [
        (68u16, "acc@ += ta.sma(syn, 5)", false),
        (69, "acc@ += ta.ema(syn + i@, 5)", true),
    ] {
        let mut r = row(
            n,
            format!(
                "{} inside a for loop",
                if is_ema { "ta.ema" } else { "ta.sma" }
            ),
            vec![
                "acc@ = 0.0".to_string(),
                "for i@ = 0 to 2".to_string(),
                format!(">{k}"),
            ],
            iv("acc@", "", if is_ema { tol(1, 5) } else { tol(0, 5) }),
        );
        r.pins = vec![REF, "pin:U13-ta-in-loop"];
        r.readings = loop_readings(is_ema);
        o.push(r);
    }
    for (n, c) in [
        (70u16, "ta.mom(synna, 5)"),
        (71, "ta.roc(synna, 5)"),
        (72, "ta.swma(synna)"),
        (73, "ta.alma(synna, 9, 0.85, 6)"),
        (74, "ta.linreg(synna, 14, 0)"),
        (75, "ta.percentrank(synna, 14)"),
    ] {
        let mut r = row(
            n,
            c.to_string(),
            vec![format!("v@ = {c}")],
            iv("v@", "", Tol::Exact),
        );
        r.pins = vec![REF, "pin:U18-ta-na-propagation"];
        o.push(r);
    }
    let mut b76 = row(
        76,
        "ta.sma over one na".into(),
        s(&[
            "float n@ = na",
            "x@ = _n == 25 ? n@ : syn",
            "v@ = ta.sma(x@, 5)",
        ]),
        iv("v@", "", tol(0, 5)),
    );
    b76.window = Window::Head(40);
    b76.pins = vec![REF, "pin:W5-sma-na"];
    b76.readings = w5_readings();
    o.push(b76);
    o
}

/// The argument a B-wide parameter takes, or `None` when `{syn, synb, syni, a literal number}`
/// cannot satisfy it.
///
/// `pin:06-b-wide-args` — a `series float` takes `syn`, a `series bool` `synb`, any `int` the literal
/// length `14`; anything else (a string, a type variable) is unsatisfiable. Optional parameters are
/// omitted ("the mandatory arity").
///
/// `pin:06-b-wide-literal-number` (lead ruling 2026-10-17) — the satisfiable set widens to "a
/// literal number": a non-`series` `float` (`ta.bb`/`ta.bbw`'s `mult`, `ta.percentile_*`'s
/// `percentage`) takes the literal `2.0`.
fn wide_arg(p: &pine_spec::Param) -> Option<&'static str> {
    if p.sig.tv != 0 {
        return None;
    }
    match (p.sig.ty.ty, p.sig.ty.qual) {
        (Ty::Float, Qual::Series) => Some("syn"),
        (Ty::Float, _) => Some("2.0"),
        (Ty::Bool, Qual::Series) => Some("synb"),
        (Ty::Int, _) => Some("14"),
        _ => None,
    }
}

/// One B-wide entry: name, call text, result types, dialects.
#[derive(Debug, Clone, PartialEq)]
pub struct Wide {
    /// `ta.<name>`.
    pub name: &'static str,
    /// The call, e.g. `ta.cci(syn, 14)`.
    pub call: String,
    /// Result types (a tuple when > 1).
    pub rets: Vec<Ty>,
    /// The first overload's dialects.
    pub dialects: Dialects,
}

/// B-wide, derived from `pine_spec` (§4.2.2): tier 1, namespace `ta`, not in [`FIXED_NAMES`] nor
/// [`IMPLICIT_BAR_READERS`], first overload satisfiable by [`wide_arg`] with plottable results;
/// sorted by name.
pub fn wide() -> Vec<Wide> {
    let mut o: Vec<Wide> = pine_spec::ENTRIES
        .iter()
        .filter(|e| e.tier == Tier::T1 && e.ns.as_str() == "ta")
        .filter(|e| !FIXED_NAMES.contains(&e.full_str()))
        .filter(|e| !IMPLICIT_BAR_READERS.iter().any(|r| r.0 == e.full_str()))
        .filter_map(|e| {
            let ov = e.overloads().first()?;
            let mut args = Vec::new();
            for p in params(ov) {
                if p.flags & (pf::OPTIONAL | pf::SYNTHETIC) != 0 {
                    continue;
                }
                args.push(wide_arg(p)?);
            }
            let rs: Vec<Ty> = rets(ov).iter().map(|r| r.ty.ty).collect();
            if rs.is_empty()
                || rs
                    .iter()
                    .any(|t| !matches!(t, Ty::Float | Ty::Int | Ty::Bool))
            {
                return None;
            }
            let ds = match (ov.versions.has(Dialect::V5), ov.versions.has(Dialect::V6)) {
                (true, true) => Dialects::BOTH,
                (true, false) => Dialects::V5,
                _ => Dialects::V6,
            };
            Some(Wide {
                name: e.full_str(),
                call: format!("{}({})", e.full_str(), args.join(", ")),
                rets: rs,
                dialects: ds,
            })
        })
        .collect();
    o.sort_by(|a, b| a.name.cmp(b.name));
    o
}

/// Plots for a value of type `t`: the isna/val pair, or `val` alone for a bool (no `na()` in v6).
fn plots_for(e: &str, t: Ty, k: &str) -> Vec<(String, String, Tol)> {
    if t == Ty::Bool {
        vec![(format!("{e} ? 1 : 0"), format!("val{k}"), Tol::Exact)]
    } else {
        iv(e, k, Tol::Exact)
    }
}

fn wide_row(n: u16, w: &Wide) -> Row {
    let (body, plots) = if w.rets.len() == 1 {
        (
            vec![format!("v@ = {}", w.call)],
            plots_for("v@", w.rets[0], ""),
        )
    } else {
        let names: Vec<String> = (1..=w.rets.len()).map(|i| format!("v{i}@")).collect();
        let mut p = Vec::new();
        for (i, t) in w.rets.iter().enumerate() {
            p.extend(plots_for(&names[i], *t, &format!("{}", i + 1)));
        }
        (vec![format!("[{}] = {}", names.join(", "), w.call)], p)
    };
    let mut r = row(n, format!("{} warm-up", w.name), body, plots);
    r.about.push("B-wide".into());
    r.ds = w.dialects;
    r
}

fn chart_row(n: u16, (name, call, k, cum): (&str, &str, u8, bool)) -> Row {
    let (body, plots, tier) = match k {
        0 => (
            vec![format!("pp@ = {call}")],
            iv("array.size(pp@)", "", Tol::Exact),
            3,
        ),
        1 if cum => (
            vec![format!("x@ = {call}"), "v@ = x@ - x@[1]".to_string()],
            iv("v@", "", Tol::Exact),
            1,
        ),
        1 => (vec![format!("v@ = {call}")], iv("v@", "", Tol::Exact), 1),
        _ => {
            let names: Vec<String> = (1..=k).map(|i| format!("v{i}@")).collect();
            let mut p = Vec::new();
            for (i, nm) in names.iter().enumerate() {
                p.extend(iv(nm, &format!("{}", i + 1), Tol::Exact));
            }
            (vec![format!("[{}] = {call}", names.join(", "))], p, 1)
        }
    };
    let mut r = row(n, format!("{name} on the chart"), body, plots);
    r.about.push("B-chart".into());
    r.prelude = Prelude::None;
    r.window = Window::Tail(256);
    r.run = chart_run();
    r.tier = tier;
    r
}

fn cell(r: Row) -> Cell {
    let id = FixtureId::from_parts(Group::B, r.n);
    let sfx = format!("_{}", id.as_str());
    let tols = r.plots.iter().map(|p| p.2).collect();
    let plots = r
        .plots
        .into_iter()
        .map(|(e, k, _)| (e.replace('@', &sfx), k))
        .collect();
    let pins: Vec<String> = r.pins.iter().map(|p| p.to_string()).collect();
    let twin = (r.twin != 0).then(|| FixtureId::from_parts(Group::B, r.twin));
    let script = Script {
        id,
        dialects: r.ds,
        about: r.about,
        twin,
        pins: pins.clone(),
        decl: None,
        prelude: r.prelude,
        body: r.body.iter().map(|s| line(s, id)).collect(),
        plots,
    };
    Cell {
        script,
        title: r.title,
        pins,
        oracle: Oracle::Tv,
        tier: r.tier,
        window: r.window,
        tols,
        expect: Expect::Pending,
        readings: r.readings,
        run: r.run,
    }
}

/// The derived counts `(B-wide, B-chart)` the generator prints (§4.2.2).
pub fn derived_counts() -> (usize, usize) {
    (wide().len(), IMPLICIT_BAR_READERS.len())
}

/// Every group-B cell: B001–B076, then B-wide, then B-chart.
pub fn cells() -> Vec<Cell> {
    let mut rows = Vec::new();
    for f in 0..10 {
        for e in 0..6 {
            rows.push(core(f, e));
        }
    }
    rows.extend(pins());
    let mut n = 77u16;
    for w in wide() {
        rows.push(wide_row(n, &w));
        n += 1;
    }
    for r in IMPLICIT_BAR_READERS {
        rows.push(chart_row(n, r));
        n += 1;
    }
    rows.into_iter().map(cell).collect()
}
