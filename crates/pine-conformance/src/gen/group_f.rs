// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Group F — arithmetic edges and other specs' pins (impl-06 §4.2.6). GENERATOR-TIME ONLY.
//!
//! COPY-DOCTRINE: generator-time reading rows; nothing here runs on a bar step.
//!
//! The core table F001–F072 (SYN-1, `align step`, `head 64`, `exact` unless stated) and the F-pin
//! block F073–F098 (`head 1`, `dialects 56` unless stated) with their readings (§4.9.4).
//!
//! Row notation: `@` in a line stands for `_<ID>` (§4.2.0 suffixed identifiers); a leading `>` is
//! one block level (indentation only through `emit::INDENT`). Each value `(type, expr)` becomes
//! `v<i>_<ID> = expr` and its plots: the isna/val pair, or, in v6, `val` alone for a bool.

use pine_ir::{DiagCode, Dialect, FaultKind, Qual};
use pine_spec::{params, Tier};

use super::emit::{Line, Prelude, Script};
use super::readings::{
    ema, every_bar, executed_only, fmod_as_written, fmod_exact, pair, proj_rgb, row as arow, sma,
    EmaSeed, NA,
};
use super::{Cell, Run};
use crate::fixture::{
    Dialects, Expect, FixtureId, Group, Oracle, Reading, ReadingBody, Row, Tol, Window,
};
use crate::syn::SynSeq;

/// What a row expects.
#[derive(Clone, Copy)]
enum X {
    /// A `tv` fixture awaiting capture.
    Tv,
    /// `refuse <code>`, authored.
    Refuse(u16),
    /// `fault <kind> 0`, authored.
    Fault(FaultKind),
}

struct R {
    n: u16,
    ds: Dialects,
    twin: u16,
    pins: &'static [&'static str],
    pre: &'static [&'static str],
    vals: &'static [(char, &'static str)],
    tol: Tol,
    window: Window,
    x: X,
    /// Plot `val` only (rows whose kernel states a plot count: F015).
    val_only: bool,
    /// An extra `cnt` plot, `exact` (`pin:06-w3-counter-plot`).
    cnt: Option<&'static str>,
}

const B: Dialects = Dialects::BOTH;
const V5: Dialects = Dialects::V5;
const V6: Dialects = Dialects::V6;
const H64: Window = Window::Head(64);
const T64: Window = Window::Tail(64);
const E: Tol = Tol::Exact;

const W1: &[&str] = &["pin:W1-int-div"];
const W3: &[&str] = &["pin:W3-lazy-and-or"];
const W13: &[&str] = &["pin:W13-bool-na"];
const W15: &[&str] = &["pin:W15-neg-index"];
const U15: &[&str] = &["pin:U15-float-div-zero"];
const MODSIGN: &[&str] = &["pin:06-mod-sign"];
const MODZERO: &[&str] = &["pin:06-mod-zero"];
const INTW: &[&str] = &["pin:06-int-width"];
const NONFIN: &[&str] = &["pin:06-nonfinite"];
const NONE: &[&str] = &[];

const fn r(n: u16, pins: &'static [&'static str], vals: &'static [(char, &'static str)]) -> R {
    R {
        n,
        ds: B,
        twin: 0,
        pins,
        pre: &[],
        vals,
        tol: E,
        window: H64,
        x: X::Tv,
        val_only: false,
        cnt: None,
    }
}

const fn rp(
    n: u16,
    pins: &'static [&'static str],
    pre: &'static [&'static str],
    vals: &'static [(char, &'static str)],
) -> R {
    R {
        n,
        ds: B,
        twin: 0,
        pins,
        pre,
        vals,
        tol: E,
        window: H64,
        x: X::Tv,
        val_only: false,
        cnt: None,
    }
}

const fn d(mut row: R, ds: Dialects, twin: u16) -> R {
    row.ds = ds;
    row.twin = twin;
    row
}

const fn x(mut row: R, ex: X) -> R {
    row.x = ex;
    row
}

const fn w3(mut row: R) -> R {
    row.cnt = Some(W3_CTR);
    row
}

const fn tol(mut row: R, t: Tol, w: Window) -> R {
    row.tol = t;
    row.window = w;
    row
}

/// `pin:06-w3-counter-plot` (lead ruling, 2026-10-17) — F030/F051 and F052/F053 observe W3
/// (whether the right operand ran) through a counter plot `cnt` beside the result. The counter is a
/// field of a `var` UDT object the right operand's function increments (a function may not assign
/// a global, `pin:02-fn-global-assign`; an object's field is not one). F030's `ta.sma(syn, 5) > 0`
/// is that function's result; F052's function keeps its `var` call counter and publishes it.
const W3_CTR: &str = "k@.n@";
const SMA_OR_PRE: &[&str] = &[
    "type Ctr@",
    ">int n@ = 0",
    "var Ctr@ k@ = Ctr@.new()",
    "rhs@(Ctr@ c@) =>",
    ">c@.n@ := c@.n@ + 1",
    ">ta.sma(syn, 5) > 0",
];
const SMA_OR: &[(char, &str)] = &[('b', "synb or rhs@(k@)")];
const BOOL_NA: &[&str] = &["bool n@ = na"];
const BOOL_NA_V: &[(char, &str)] = &[('b', "n@ and false"), ('b', "n@ or true")];
const COUNT_PRE: &[&str] = &[
    "type Ctr@",
    ">int n@ = 0",
    "var Ctr@ k@ = Ctr@.new()",
    "f_count@() =>",
    ">var int c@ = 0",
    ">c@ += 1",
    ">k@.n@ := c@",
    ">c@ > 0",
];
const COUNT_V: &[(char, &str)] = &[('b', "synb and f_count@()")];
const CONST52: &[&str] = &["const int a@ = 5", "const int b@ = 2"];
const CONST52_V: &[(char, &str)] = &[('f', "a@ / b@ + (syn - syn)")];
const CONST7: &[&str] = &["const int a@ = -7"];
const CONST7_V: &[(char, &str)] = &[('f', "a@ / 2")];
const FEQ: &[(char, &str)] = &[
    ('b', "0.1 + 0.2 == 0.3"),
    ('b', "1.0000000001 == 1.0"),
    ('b', "1e-10 == 2e-10"),
];
const ARR: &str = "arr@ = array.from(1.0, 2.0, 3.0)";
const GET_NEG: &[&str] = &[ARR];
const GET_NEG_V: &[(char, &str)] = &[('f', "array.get(arr@, -1)")];
const SET_NEG: &[&str] = &[ARR, "array.set(arr@, -1, 9.0)"];
const INS_NEG: &[&str] = &[ARR, "array.insert(arr@, -1, 9.0)"];
const AT2: &[(char, &str)] = &[('f', "array.get(arr@, 2)")];
const REM_NEG: &[&str] = &[ARR, "array.remove(arr@, -1)"];
const SIZE: &[(char, &str)] = &[('i', "array.size(arr@)")];

/// `pin:06-f-w15-v5-fault` (confirmed by the lead, 2026-10-17) — F067/F069/F071 (v5 `set`/`insert`/`remove` at −1) expect the same
/// `fault CollectionBounds 0` §4.2.6 states for F065 (W15: v5 refuses a negative index at runtime).
/// `pin:06-f030-shape`, `pin:06-f052-shape` — F030 plots the `or` result and F052 the `and` result
/// as written (§4.2.6 names the kernel only), each with `pin:06-w3-counter-plot`'s `cnt` beside
/// it; `pin:06-f042-operands` — F042's `x` starts at `syni` / `syn` so the `+=` is not folded; `pin:06-f048-arms` — F048's two arms are `-1`, `-2`
/// (`syni ∈ [0, 1000)` never matches).
fn rows() -> Vec<R> {
    vec![
        r(1, W1, &[('f', "syni / 7")]),
        r(2, W1, &[('f', "(0 - syni) / 7")]),
        r(3, W1, &[('f', "syni / (0 - 7)")]),
        r(4, MODSIGN, &[('i', "syni % 7"), ('i', "(0 - syni) % 7")]),
        r(5, U15, &[('f', "syni / (syni - syni)")]),
        r(6, MODZERO, &[('i', "syni % (syni - syni)")]),
        r(7, U15, &[('f', "syn / 0.0")]),
        r(8, U15, &[('f', "(syn - syn) / (syn - syn)")]),
        r(9, U15, &[('f', "1.0 / ((syn - syn) * -1.0)")]),
        r(10, MODZERO, &[('f', "syn % 0.0")]),
        r(11, INTW, &[('i', "9223372036854775807 + syni")]),
        r(12, INTW, &[('i', "-9223372036854775808")]),
        r(13, INTW, &[('f', "float(9007199254740993)")]),
        r(
            14,
            &["pin:02-int-cast"],
            &[
                ('i', "int(2.5)"),
                ('i', "int(-2.5)"),
                ('i', "int(2.7)"),
                ('i', "int(-2.7)"),
            ],
        ),
        R {
            val_only: true,
            ..r(
                15,
                &["pin:U04-round-ties"],
                &[
                    ('i', "math.round(0.5)"),
                    ('i', "math.round(-0.5)"),
                    ('i', "math.round(1.5)"),
                    ('i', "math.round(-1.5)"),
                    ('i', "math.round(2.5)"),
                    ('i', "math.round(-2.5)"),
                ],
            )
        },
        // pin:06-f016-no-tie (lead ruling) — `math.round(syn, 3)` stays literal: `syn` has two
        // decimals, so no step is a tie at 3; the row confirms the 3-decimal path, no tie invented.
        r(16, &["pin:U04-round-ties"], &[('f', "math.round(syn, 3)")]),
        r(
            17,
            NONE,
            &[('i', "math.floor(-2.5)"), ('i', "math.ceil(-2.5)")],
        ),
        r(18, NONE, &[('f', "math.sign((syn - syn) * -1.0)")]),
        r(19, NONE, &[('f', "0.1 + 0.2 + (syn - syn)")]),
        r(20, NONFIN, &[('f', "1e308 * 10 + (syn - syn)")]),
        r(21, NONE, &[('f', "math.pow(0, 0)")]),
        r(22, NONE, &[('f', "math.pow(-8, 1.0 / 3.0)")]),
        r(23, NONE, &[('f', "math.sqrt(0.0 - 1.0)")]),
        r(
            24,
            NONFIN,
            &[('f', "math.log(0.0)"), ('f', "math.log(0.0 - 1.0)")],
        ),
        r(25, NONFIN, &[('f', "math.exp(710.0)")]),
        r(26, NONFIN, &[('b', "syn > 1e308 * 10")]),
        rp(
            27,
            &["pin:06-minmax-na"],
            &["float n@ = na"],
            &[('f', "math.max(n@, 1.0)"), ('f', "math.min(n@, 1.0)")],
        ),
        r(
            28,
            NONE,
            &[('b', "not true and false"), ('b', "not (true and false)")],
        ),
        r(29, NONE, &[('f', "0 - syn * 2"), ('f', "(0 - syn) * 2")]),
        w3(d(rp(30, W3, SMA_OR_PRE, SMA_OR), V5, 51)),
        // F031 is spelled by `f031_body` (§4.2.6's example, byte for byte).
        r(31, &["pin:U14-ternary-unselected-arm"], &[]),
        r(32, NONE, &[('s', "\"x\" + str.tostring(syni)")]),
        r(
            33,
            NONE,
            &[('b', "\"a\" == \"a\""), ('b', "\"a\" == \"A\"")],
        ),
        r(34, NONE, &[('b', "color.rgb(1,2,3) == color.rgb(1,2,3)")]),
        d(rp(35, W13, BOOL_NA, BOOL_NA_V), V5, 54),
        rp(36, NONE, &["float f@ = 3"], &[('f', "f@")]),
        r(37, INTW, &[('i', "3037000500 * 3037000500")]),
        tol(
            r(38, NONE, &[('f', "math.todegrees(math.toradians(syn))")]),
            Tol::Ulp(2),
            H64,
        ),
        r(
            39,
            MODSIGN,
            &[('f', "syn % 7.0"), ('f', "(0.0 - syn) % 7.0")],
        ),
        r(40, INTW, &[('i', "math.abs(-9223372036854775807 - 1)")]),
        r(41, U15, &[('f', "syn / (syni < 500 ? 1 : 0)")]),
        rp(
            42,
            NONE,
            &["int xi@ = syni", "xi@ += 1", "float xf@ = syn", "xf@ += 1"],
            &[('i', "xi@"), ('f', "xf@")],
        ),
        tol(
            r(
                43,
                NONE,
                &[
                    ('f', "math.avg(syn, syn[1], syn[2])"),
                    ('f', "(syn + syn[1] + syn[2]) / 3"),
                ],
            ),
            Tol::Ulp(2),
            H64,
        ),
        tol(
            r(44, &["pin:00-dataset-start"], &[('f', "ta.cum(syn)")]),
            Tol::Ulp(32768),
            T64,
        ),
        tol(
            r(
                45,
                &["pin:00-dataset-start"],
                &[('f', "math.sum(syn, 500)")],
            ),
            Tol::Ulp(65536),
            T64,
        ),
        rp(
            46,
            NONE,
            &["float n@ = na"],
            &[('f', "nz(n@) + 1"), ('f', "nz(n@ + 1)")],
        ),
        r(
            47,
            INTW,
            &[
                ('b', "9007199254740993 == 9007199254740992"),
                ('b', "9007199254740993.0 == 9007199254740992.0"),
            ],
        ),
        rp(
            48,
            NONE,
            &["w@ = switch syni", ">-1 => 1", ">-2 => 2"],
            &[('i', "w@")],
        ),
        rp(
            49,
            NONE,
            &["int cnt@ = 0", "for i@ = 0.0 to 1.0 by 0.1", ">cnt@ += 1"],
            &[('i', "cnt@")],
        ),
        rp(
            50,
            NONE,
            &[
                "int i@ = 1000 + (syni - syni)",
                "int cnt@ = 0",
                "while i@ > 0",
                ">i@ := int(i@ / 2)",
                ">cnt@ += 1",
            ],
            &[('i', "cnt@")],
        ),
        w3(d(rp(51, W3, SMA_OR_PRE, SMA_OR), V6, 30)),
        w3(d(rp(52, W3, COUNT_PRE, COUNT_V), V5, 53)),
        w3(d(rp(53, W3, COUNT_PRE, COUNT_V), V6, 52)),
        x(d(rp(54, W13, BOOL_NA, BOOL_NA_V), V6, 35), X::Refuse(3007)),
        d(rp(55, W1, CONST52, CONST52_V), V5, 56),
        d(rp(56, W1, CONST52, CONST52_V), V6, 55),
        d(rp(57, W1, CONST7, CONST7_V), V5, 58),
        d(rp(58, W1, CONST7, CONST7_V), V6, 57),
        x(
            rp(59, W1, &["int x@ = syni / 7"], &[('i', "x@")]),
            X::Refuse(3007),
        ),
        rp(
            60,
            &["pin:U05-int-of-na"],
            &["float n@ = na"],
            &[('i', "int(n@)")],
        ),
        d(r(61, &["pin:U06-float-eq-v6"], FEQ), V5, 62),
        d(r(62, &["pin:U06-float-eq-v6"], FEQ), V6, 61),
        r(
            63,
            &["pin:U07-str-match-none"],
            &[('s', "str.match(str.tostring(syni), \"x+\")")],
        ),
        r(
            64,
            &["pin:U08-tostring-precision"],
            &[
                ('i', "str.length(str.tostring(syn / 3.0))"),
                ('i', "str.length(str.tostring(syn))"),
            ],
        ),
        x(
            d(rp(65, W15, GET_NEG, GET_NEG_V), V5, 66),
            X::Fault(FaultKind::CollectionBounds),
        ),
        d(rp(66, W15, GET_NEG, GET_NEG_V), V6, 65),
        x(
            d(rp(67, W15, SET_NEG, AT2), V5, 68),
            X::Fault(FaultKind::CollectionBounds),
        ),
        d(rp(68, W15, SET_NEG, AT2), V6, 67),
        x(
            d(rp(69, W15, INS_NEG, AT2), V5, 70),
            X::Fault(FaultKind::CollectionBounds),
        ),
        d(rp(70, W15, INS_NEG, AT2), V6, 69),
        x(
            d(rp(71, W15, REM_NEG, SIZE), V5, 72),
            X::Fault(FaultKind::CollectionBounds),
        ),
        d(rp(72, W15, REM_NEG, SIZE), V6, 71),
    ]
}

/// F031, §4.2.6's example body (its column alignment is part of the text).
fn f031_body() -> Vec<Line> {
    [
        "probe_F031 = synb ? ta.sma(syn, 5) : 0.0",
        "ref_F031   = ta.sma(syn, 5)",
        "d_F031     = na(probe_F031) or na(ref_F031) ? na : probe_F031 - ref_F031",
    ]
    .into_iter()
    .map(Line::top)
    .collect()
}

fn line(s: &str, id: FixtureId) -> Line {
    let sfx = format!("_{}", id.as_str());
    match s.strip_prefix('>') {
        Some(rest) => Line::block(rest.replace('@', &sfx)),
        None => Line::top(s.replace('@', &sfx)),
    }
}

fn proj(t: char, v: &str) -> String {
    match t {
        'b' => format!("({v} ? 1 : 0)"),
        's' => format!("str.length({v})"),
        _ => v.to_string(),
    }
}

fn cell(row: &R) -> Cell {
    let id = FixtureId::from_parts(Group::F, row.n);
    let mut body: Vec<Line> = row.pre.iter().map(|s| line(s, id)).collect();
    let mut plots = Vec::new();
    let mut tols = Vec::new();
    let multi = row.vals.len() > 1;
    for (i, (t, expr)) in row.vals.iter().enumerate() {
        let v = format!("v{}_{}", i + 1, id.as_str());
        body.push(line(&format!("{v} = {expr}"), id));
        let k = if multi {
            format!("{}", i + 1)
        } else {
            String::new()
        };
        // A bool has no `na()` in v6 (W13), so a 56 or v6 bool plots `val` alone.
        if *t == 'b' && row.ds != V5 {
            plots.push((format!("{v} ? 1 : 0"), format!("val{k}")));
            tols.push(Tol::Exact);
        } else if row.val_only {
            plots.push((v.clone(), format!("val{k}")));
            tols.push(row.tol);
        } else {
            plots.push((format!("na({v}) ? 1 : 0"), format!("isna{k}")));
            plots.push((
                format!("na({v}) ? na : {}", proj(*t, &v)),
                format!("val{k}"),
            ));
            tols.push(Tol::Exact);
            tols.push(row.tol);
        }
    }
    if let Some(e) = row.cnt {
        plots.push((
            e.replace('@', &format!("_{}", id.as_str())),
            "cnt".to_string(),
        ));
        tols.push(Tol::Exact);
    }
    if row.n == 31 {
        body = f031_body();
        let dv = "d_F031".to_string();
        plots = vec![
            (format!("na({dv}) ? 1 : 0"), "isna".to_string()),
            (format!("na({dv}) ? na : {dv}"), "val".to_string()),
        ];
        tols = vec![Tol::Exact, Tol::Exact];
    }
    let (expect, oracle) = match row.x {
        X::Tv => (Expect::Pending, Oracle::Tv),
        X::Refuse(c) => (Expect::Refuse { code: DiagCode(c) }, Oracle::Authored),
        X::Fault(kind) => (Expect::Fault { kind, row: 0 }, Oracle::Authored),
    };
    let pins: Vec<String> = row.pins.iter().map(|p| p.to_string()).collect();
    let twin = (row.twin != 0).then(|| FixtureId::from_parts(Group::F, row.twin));
    let title = row
        .vals
        .iter()
        .map(|(_, e)| e.replace('@', ""))
        .collect::<Vec<_>>()
        .join(", ");
    // pin:06-f-titles: a row's title is its values as written (`@` dropped); F031's is §4.2.6's.
    let title = if row.n == 31 {
        "does a ta.* call in an untaken ternary arm advance?".to_string()
    } else {
        title
    };
    let script = Script {
        id,
        dialects: row.ds,
        about: vec![title.clone()],
        twin,
        pins: pins.clone(),
        decl: None,
        prelude: Prelude::Syn1,
        body,
        plots,
    };
    let readings = if row.n == 31 {
        f031_readings()
    } else {
        Vec::new()
    };
    Cell {
        script,
        title,
        pins,
        oracle,
        tier: if (65..=72).contains(&row.n) { 3 } else { 1 },
        window: row.window,
        tols,
        expect,
        readings,
        run: Run::default(),
    }
}

/// The first `n` SYN-1 steps: `(syn, synb)`.
fn syn_steps(n: usize) -> (Vec<f64>, Vec<bool>) {
    let mut q = SynSeq::new();
    (0..n)
        .map(|_| {
            let s = q.step();
            (s.syn, s.synb)
        })
        .unzip()
}

fn rows_of(n: usize, f: impl Fn(usize) -> Vec<f64>) -> Vec<Row> {
    (0..n).map(|i| arow(i as i64, &f(i))).collect()
}

fn reading(name: &str, body: ReadingBody) -> Reading {
    Reading {
        name: name.to_string(),
        body,
    }
}

/// F031's readings (§4.2.6, impl-00 v4 §3.11 names): the arm's `ta.sma(syn, 5)` advances only on
/// the `synb` steps (`selected_only`) or on every step (`both`); an unselected step yields `0.0`,
/// so `d = −ref` there under both readings.
fn f031_readings() -> Vec<Reading> {
    let (x, c) = syn_steps(64);
    let refv = sma(&x, 5);
    let d = |arm: &[f64]| {
        rows_of(64, |i| {
            let probe = if c[i] { arm[i] } else { 0.0 };
            let v = if probe.is_nan() || refv[i].is_nan() {
                NA
            } else {
                probe - refv[i]
            };
            pair(v).to_vec()
        })
    };
    vec![
        reading(
            "selected_only",
            ReadingBody::Rows(d(&executed_only(&x, &c, |v| sma(v, 5)))),
        ),
        reading(
            "both",
            ReadingBody::Rows(d(&every_bar(&x, &c, |v| sma(v, 5)))),
        ),
    ]
}

/// The `ta.*` F083 calls as `<L>(syn, len)`: the first tier-1 `ta.*` (in `pine_spec::ENTRIES`
/// order) whose `length` parameter is `simple` in both dialects and whose call shape is
/// `(source, length)` (`simple_len_ta_exists_in_spec`).
///
/// `pin:06-simple-len-ta-shape` — `ta.atr` precedes it with a `simple` length but takes no source,
/// so `<L>(syn, len)` could not call it; the shape is part of the selection.
pub const SIMPLE_LEN_TA: &str = "ta.ema";

/// Whether `e` qualifies as [`SIMPLE_LEN_TA`]: tier 1, namespace `ta`, and in each dialect every
/// visible overload's parameters are `source` then a `simple` `length`.
pub fn is_simple_len_ta(e: &pine_spec::Entry) -> bool {
    if e.tier != Tier::T1 || e.ns.as_str() != "ta" {
        return false;
    }
    [Dialect::V5, Dialect::V6].iter().all(|&d| {
        let ovs: Vec<_> = e.overloads().iter().filter(|o| o.versions.has(d)).collect();
        !ovs.is_empty()
            && ovs.iter().all(|o| {
                let ps: Vec<_> = params(o).iter().filter(|p| p.versions.has(d)).collect();
                ps.len() >= 2
                    && ps[0].name.as_str() == "source"
                    && ps[1].name.as_str() == "length"
                    && ps[1].sig.ty.qual == Qual::Simple
            })
    })
}

/// One F-pin row: the body (`@` → `_<ID>`, `>` one block level), the plots, the readings.
struct Fp {
    n: u16,
    ds: Dialects,
    twin: u16,
    pin: &'static str,
    title: &'static str,
    decl: Option<&'static str>,
    pre: &'static [&'static str],
    plots: Vec<(String, String)>,
    window: Window,
    x: X,
    readings: Vec<Reading>,
}

/// An F-pin row with the block's defaults: `dialects 56`, `head 1`, `tv`.
fn fp(
    n: u16,
    pin: &'static str,
    title: &'static str,
    pre: &'static [&'static str],
    plots: Vec<(String, String)>,
    readings: Vec<Reading>,
) -> Fp {
    Fp {
        n,
        ds: B,
        twin: 0,
        pin,
        title,
        decl: None,
        pre,
        plots,
        window: Window::Head(1),
        x: X::Tv,
        readings,
    }
}

/// `pin:06-fpin-plots` — where §4.2.6's F-pin row says "plot(s) X" the fixture plots exactly X
/// (`val`, or `val<k>`); where it names values, each value gets §4.2.0's isna/val pair.
fn vals(es: &[&str]) -> Vec<(String, String)> {
    let multi = es.len() > 1;
    let mut o = Vec::new();
    for (i, e) in es.iter().enumerate() {
        let k = if multi {
            format!("{}", i + 1)
        } else {
            String::new()
        };
        o.push((format!("na({e}) ? 1 : 0"), format!("isna{k}")));
        o.push((format!("na({e}) ? na : {e}"), format!("val{k}")));
    }
    o
}

fn plots_of(es: &[&str]) -> Vec<(String, String)> {
    let multi = es.len() > 1;
    es.iter()
        .enumerate()
        .map(|(i, e)| {
            let k = if multi {
                format!("{}", i + 1)
            } else {
                String::new()
            };
            (e.to_string(), format!("val{k}"))
        })
        .collect()
}

/// A `head 1` reading of the isna/val pairs of `vs`.
fn pairs1(name: &str, vs: &[f64]) -> Reading {
    let cells: Vec<f64> = vs.iter().flat_map(|&v| pair(v)).collect();
    reading(name, ReadingBody::Rows(vec![arow(0, &cells)]))
}

/// A `head 1` reading of plain plots.
fn plain1(name: &str, vs: &[f64]) -> Reading {
    reading(name, ReadingBody::Rows(vec![arow(0, vs)]))
}

fn refuse(name: &str, c: u16) -> Reading {
    reading(name, ReadingBody::Refuse(DiagCode(c)))
}

fn fault(name: &str, row: i64) -> Reading {
    reading(
        name,
        ReadingBody::Fault {
            kind: FaultKind::ScriptRefused,
            row,
        },
    )
}

const PROJ_COLORS: [&str; 3] = [
    "color.r(color.red) * 65536 + color.g(color.red) * 256 + color.b(color.red)",
    "color.r(color.teal) * 65536 + color.g(color.teal) * 256 + color.b(color.teal)",
    "color.r(color.yellow) * 65536 + color.g(color.yellow) * 256 + color.b(color.yellow)",
];
const TF_S: [&str; 3] = [
    "str.length(timeframe.from_seconds(86400))",
    "str.length(timeframe.from_seconds(604800))",
    "str.length(timeframe.from_seconds(3600))",
];
const FOR_BOUND: &[&str] = &[
    "int hi@ = 3 + (syni - syni)",
    "int cnt@ = 0",
    "for i@ = 0 to hi@",
    ">cnt@ += 1",
    ">hi@ := 6",
];
const VAR_Q: &[&str] = &["len@ = 5", "len@ := 6", "e@ = ta.ema(syn, len@)"];
const HIST_PRE: &[&str] = &["int k@ = _n > 10 ? -1 : 1", "e@ = syn[k@]"];
const HIST_NA_PRE: &[&str] = &["int nk@ = na", "int k@ = _n > 10 ? nk@ : 1", "e@ = syn[k@]"];
const HIST_FAR_PRE: &[&str] = &["int k@ = _n > 10 ? 6000 : 1", "e@ = syn[k@]"];
const EMA5: &[&str] = &["e@ = ta.ema(syn, 5)"];
const F096_DECL: &str = "indicator(\"F096\", shorttitle = \"s096\", overlay = true, format = format.price, precision = 4, scale = scale.left, explicit_plot_zorder = true, max_lines_count = 10, max_labels_count = 10, max_boxes_count = 10, max_polylines_count = 10)";
const F098_OPS: [(f64, f64); 6] = [
    (1.0, 0.1),
    (10.0, 0.1),
    (-0.3, 0.1),
    (1.0, 0.1),
    (10.0, 0.1),
    (-0.3, 0.1),
];

/// F087–F089's `na` reading: `syn[1]` on rows 0–9 (`na` on row 0), `na` from row 10 (`_n > 10`).
fn hist_readings() -> Vec<Reading> {
    let (x, _) = syn_steps(16);
    let rows = rows_of(16, |i| {
        let v = if i == 0 || i >= 10 { NA } else { x[i - 1] };
        pair(v).to_vec()
    });
    vec![
        reading("na", ReadingBody::Rows(rows)),
        fault("runtime_error", 10),
    ]
}

/// F098's readings over its seven plots at row 0.
fn f098_reading(name: &str, f: fn(f64, f64) -> f64) -> Reading {
    let (x, _) = syn_steps(1);
    let mut v: Vec<f64> = F098_OPS.iter().map(|&(a, b)| f(a, b)).collect();
    v.push(f(x[0], 0.25));
    plain1(name, &v)
}

/// F073–F098 (§4.2.6 F-pin). Readings' rows are authored here from the literal values of the
/// table (§4.9.4); the order is the table's (the owner's default first where it names one).
///
/// `pin:06-f083-join-ema-default` — F083's `join` row is `ta.ema(syn, 6)` on row 0 under
/// `pin:U01-ema-seed`'s default: `sma` since CI-014 (r33a), so `na` until six values exist (r37
/// re-distil, R-P26; it was `first_value`'s `syn` itself). `pin:06-f088-typed-na` — F088's
/// typed `na` offset is a declared `int` holding `na`. `pin:06-f092-one-reading` — F092's second
/// reading ("the printed length") has no name and no value we may author (no TradingView-derived
/// values), so only `na` is written; a printed capture decides `Neither` and escalates.
/// `pin:06-f090-range-only` — F090 carries no reading (`pin:04-math-random`: range only).
fn fpins() -> Vec<Fp> {
    let (x1, _) = syn_steps(1);
    let red = [0xFF5252, 0x00897B, 0xFFEB3B].map(proj_rgb);
    let red6 = [0xF23645, 0x089981, 0xFDD835].map(proj_rgb);
    let colors = || vec![plain1("v5_values", &red), plain1("v6_values", &red6)];
    let tf = || {
        vec![
            pairs1("omit_one", &[1.0, 1.0, 2.0]),
            pairs1("always", &[2.0, 2.0, 2.0]),
        ]
    };
    let forb = || {
        vec![
            plain1("fixed_at_start", &[4.0]),
            plain1("each_iteration", &[7.0]),
        ]
    };
    let join = ema(&x1, 6, EmaSeed::Sma)[0];
    vec![
        Fp {
            ds: V5,
            twin: 74,
            ..fp(73, "pin:00-color-consts", "PROJ of color.red, color.teal, color.yellow", &[], plots_of(&PROJ_COLORS), colors())
        },
        Fp {
            ds: V6,
            twin: 73,
            ..fp(74, "pin:00-color-consts", "PROJ of color.red, color.teal, color.yellow", &[], plots_of(&PROJ_COLORS), colors())
        },
        Fp {
            ds: V5,
            twin: 76,
            ..fp(75, "pin:00-tf-multiplier", "str.length(timeframe.from_seconds(s)), s in 86400, 604800, 3600", &[], vals(&TF_S), tf())
        },
        Fp {
            ds: V6,
            twin: 75,
            ..fp(76, "pin:00-tf-multiplier", "str.length(timeframe.from_seconds(s)), s in 86400, 604800, 3600", &[], vals(&TF_S), tf())
        },
        Fp {
            ds: V5,
            twin: 78,
            ..fp(77, "pin:00-for-bound", "for bound reassigned inside the loop", FOR_BOUND, plots_of(&["cnt@"]), forb())
        },
        Fp {
            ds: V6,
            twin: 77,
            ..fp(78, "pin:00-for-bound", "for bound reassigned inside the loop", FOR_BOUND, plots_of(&["cnt@"]), forb())
        },
        fp(
            79,
            "pin:02-shadow-builtin",
            "a user function named nz",
            &["nz(x@) => x@ + 1"],
            plots_of(&["nz(1)"]),
            vec![refuse("refuse", 3004), plain1("shadow", &[2.0])],
        ),
        fp(
            80,
            "pin:02-color-alpha",
            "color.t of #FF000080, #FF000040, #FF0000C0, #FF000001",
            &[],
            vals(&[
                "color.t(#FF000080)",
                "color.t(#FF000040)",
                "color.t(#FF0000C0)",
                "color.t(#FF000001)",
            ]),
            vec![
                pairs1("round_half_up", &[50.0, 75.0, 25.0, 100.0]),
                pairs1("truncate", &[49.0, 74.0, 24.0, 99.0]),
            ],
        ),
        Fp {
            ds: V5,
            ..fp(
                81,
                "pin:02-na-condition",
                "an na bool as a ternary, if and switch condition",
                &[
                    "bool b@ = na",
                    "x@ = b@ ? 1 : 2",
                    "y@ = if b@",
                    ">1",
                    "else",
                    ">2",
                    "z@ = switch",
                    ">b@ => 1",
                    ">=> 2",
                ],
                vals(&["x@", "y@", "z@"]),
                vec![
                    pairs1("as_false", &[2.0, 2.0, 2.0]),
                    pairs1("na_result", &[NA, NA, NA]),
                ],
            )
        },
        Fp {
            ds: V5,
            ..fp(
                82,
                "pin:02-v5-num-to-bool",
                "bool(f) of an na float",
                &["float f@ = na", "b@ = bool(f@)"],
                plots_of(&["na(b@) ? 1 : 0", "na(b@) ? -1 : b@ ? 1 : 0"]),
                vec![plain1("na", &[1.0, -1.0]), plain1("false", &[0.0, 0.0])],
            )
        },
        Fp {
            ds: V5,
            twin: 84,
            ..fp(
                83,
                "pin:02-var-qualifier",
                "a reassigned length passed to a simple length parameter",
                VAR_Q,
                vals(&["e@"]),
                vec![refuse("series", 3008), pairs1("join", &[join])],
            )
        },
        Fp {
            ds: V6,
            twin: 83,
            x: X::Refuse(3008),
            ..fp(
                84,
                "pin:02-var-qualifier",
                "a reassigned length passed to a simple length parameter",
                VAR_Q,
                vals(&["e@"]),
                Vec::new(),
            )
        },
        fp(
            85,
            "pin:02-for-step",
            "for with a negative series step",
            &[
                "int s@ = -2 + (syni - syni)",
                "cnt@ = 0",
                "for i@ = 0 to 10 by s@",
                ">cnt@ += 1",
            ],
            plots_of(&["cnt@"]),
            vec![refuse("refuse_dynamic", 3008), plain1("runtime_abs", &[6.0])],
        ),
        fp(
            86,
            "pin:02-fn-global-assign",
            "a function assigns a global var",
            &["var int g@ = 0", "f@() => g@ := g@ + 1", "f@()"],
            plots_of(&["g@"]),
            vec![refuse("refuse", 3040), plain1("allow", &[1.0])],
        ),
        Fp {
            window: Window::Head(16),
            ..fp(87, "pin:03-hist-dynamic-offset", "a negative history offset from row 10", HIST_PRE, vals(&["e@"]), hist_readings())
        },
        Fp {
            window: Window::Head(16),
            ..fp(88, "pin:03-hist-dynamic-offset", "a typed na history offset from row 10", HIST_NA_PRE, vals(&["e@"]), hist_readings())
        },
        Fp {
            window: Window::Head(16),
            ..fp(89, "pin:03-hist-dynamic-offset", "a history offset of 6000 from row 10", HIST_FAR_PRE, vals(&["e@"]), hist_readings())
        },
        Fp {
            window: Window::Head(64),
            ..fp(
                90,
                "pin:04-math-random",
                "math.random(5, 10, 7) stays in range",
                &["r@ = math.random(5, 10, 7)"],
                plots_of(&["r@ >= 5 and r@ < 10 ? 1 : 0"]),
                Vec::new(),
            )
        },
        fp(
            91,
            "pin:04-time-edges",
            "timeframe.change on the first execution",
            &[],
            plots_of(&["timeframe.change(\"D\") ? 1 : 0"]),
            vec![plain1("first_true", &[1.0]), plain1("first_false", &[0.0])],
        ),
        fp(
            92,
            "pin:04-time-edges",
            "timeframe.from_seconds(7)",
            &["x@ = timeframe.from_seconds(7)"],
            plots_of(&["na(x@) ? -1 : str.length(x@)"]),
            vec![plain1("na", &[-1.0])],
        ),
        fp(
            93,
            "pin:04-time-edges",
            "timestamp with month 13 and February 30",
            &[],
            vals(&[
                "(timestamp(\"Etc/UTC\", 2024, 13, 1, 0, 0) - timestamp(\"Etc/UTC\", 2025, 1, 1, 0, 0)) / 1000",
                "(timestamp(\"Etc/UTC\", 2024, 2, 30, 0, 0) - timestamp(\"Etc/UTC\", 2024, 3, 1, 0, 0)) / 1000",
            ]),
            vec![pairs1("carry", &[0.0, 0.0]), fault("runtime_error", 0)],
        ),
        fp(
            94,
            "pin:04-time-edges",
            "weekofyear of 2021-01-01, a Friday",
            &[],
            vals(&["weekofyear(timestamp(\"Etc/UTC\", 2021, 1, 1, 0, 0), \"Etc/UTC\")"]),
            vec![pairs1("iso_8601", &[53.0]), pairs1("jan1_week1", &[1.0])],
        ),
        fp(
            95,
            "pin:04-time-edges",
            "hour in an unknown series time zone",
            &["string tz@ = syni >= 0 ? \"Mars/Olympus\" : \"Etc/UTC\""],
            vals(&["hour(timestamp(\"Etc/UTC\", 2024, 1, 1, 5, 0), tz@)"]),
            vec![fault("runtime_error", 0), pairs1("utc_fallback", &[5.0])],
        ),
        Fp {
            decl: Some(F096_DECL),
            window: Window::Head(64),
            ..fp(96, "pin:10-shell-decl-args", "ta.ema(syn, 5) under every inert indicator argument", EMA5, vals(&["e@"]), Vec::new())
        },
        Fp {
            decl: Some("strategy(\"F097\")"),
            window: Window::Head(64),
            ..fp(97, "pin:10-shell-decl-args", "ta.ema(syn, 5) under strategy()", EMA5, vals(&["e@"]), Vec::new())
        },
        fp(
            98,
            "pin:03-float-mod",
            "float % of a non-representable divisor, folded and series",
            &[],
            plots_of(&[
                "1.0 % 0.1",
                "10.0 % 0.1",
                "-0.3 % 0.1",
                "(1.0 + (syn - syn)) % 0.1",
                "(10.0 + (syn - syn)) % 0.1",
                "(-0.3 + (syn - syn)) % 0.1",
                "syn % 0.25",
            ]),
            vec![
                f098_reading("as_written", fmod_as_written),
                f098_reading("exact_remainder", fmod_exact),
            ],
        ),
    ]
}

fn fpin_cell(p: Fp) -> Cell {
    let id = FixtureId::from_parts(Group::F, p.n);
    let sfx = format!("_{}", id.as_str());
    let body: Vec<Line> = p.pre.iter().map(|s| line(s, id)).collect();
    let plots: Vec<(String, String)> = p
        .plots
        .into_iter()
        .map(|(e, k)| (e.replace('@', &sfx), k))
        .collect();
    let tols = vec![Tol::Exact; plots.len()];
    let (expect, oracle) = match p.x {
        X::Tv => (Expect::Pending, Oracle::Tv),
        X::Refuse(c) => (Expect::Refuse { code: DiagCode(c) }, Oracle::Authored),
        X::Fault(kind) => (Expect::Fault { kind, row: 0 }, Oracle::Authored),
    };
    let pins = vec![p.pin.to_string()];
    let twin = (p.twin != 0).then(|| FixtureId::from_parts(Group::F, p.twin));
    let script = Script {
        id,
        dialects: p.ds,
        about: vec![p.title.to_string()],
        twin,
        pins: pins.clone(),
        decl: p.decl.map(str::to_string),
        prelude: Prelude::Syn1,
        body,
        plots,
    };
    Cell {
        script,
        title: p.title.to_string(),
        pins,
        oracle,
        tier: 1,
        window: p.window,
        tols,
        expect,
        readings: p.readings,
        run: Run::default(),
    }
}

/// Every group-F cell: F001–F098, then r29's F099–F119 (`group_f2`) and r34's F120–F128
/// (`group_f3`), appended so no captured id moves.
pub fn cells() -> Vec<Cell> {
    let mut c: Vec<Cell> = rows().iter().map(cell).collect();
    c.extend(fpins().into_iter().map(fpin_cell));
    c.extend(super::group_f2::cells());
    c.extend(super::group_f3::cells());
    c
}
