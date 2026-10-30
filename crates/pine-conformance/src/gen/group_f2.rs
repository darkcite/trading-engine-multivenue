// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Group F, r29 block (F099–F119): the fixtures of the next TradingView capture session, appended
//! after F098 so no captured id moves (I6). GENERATOR-TIME ONLY.
//!
//! COPY-DOCTRINE: generator-time reading rows; nothing here runs on a bar step.
//!
//! * **Tier 3 (R-P5)** — one fixture per behaviour cluster of the rows pine-std serves (array
//!   core, sort, statistics, every / some, binary search, map, the 2026 UDT field rows, the
//!   method form): F099–F114. The UDT field overloads are `pine_spec` v6-only, so each v6 fixture has its §4.3
//!   rule-2 twin in v5 expecting `refuse 3005` (F109, F111, F113).
//! * **R-P15** — the `p = 25` percentile probes that separate `C = 1/2` from `C = 0` (and
//!   pine-std's `array.*` `C = 1`): F103 (`array.*`, tier 3) and F104 (`ta.*`, tier 1).
//! * **Open issues** — CI-019 (`%` sign: F115 int, F116 float), CI-022 (F019's association:
//!   F117), the R-P2 sema-scope probes (F118 `syn / (2 - 2)`, F119 a `for` with an `na` step; the
//!   `strategy.order` one is E128).
//! * **R-P16** (CI-052) — F108 re-authored without its `binary_search*` field lines (TradingView
//!   refuses them at compile); pending. **R-P17** (CI-053) — F119's `one_pass` reading.
//!
//! SYN-1, `align step`, `exact` unless a plot says otherwise. Row notation as group F: `@` stands
//! for `_<ID>`, a leading `>` is one block level. Values whose type admits `na()` get §4.2.0's
//! isna/val pair; a v6 / `56` bool plots `val` alone; pin-reading fixtures plot their values plain
//! (§4.2.6 F-pin's `plots_of`), so each reading is one short row.

use pine_ir::{DiagCode, FaultKind};

use super::emit::{Line, Prelude, Script};
use super::readings::{
    bsearch, fmod_exact, heap_sort_order, mod_floored, mod_truncated, pair, pct_linear,
    row as arow, stable_sort_order, ta_pct_linear, LeftBelow, PctC, NA,
};
use super::{Cell, Run};
use crate::fixture::{
    Dialects, Expect, FixtureId, Group, Oracle, Reading, ReadingBody, Row, Tol, Window,
};
use crate::syn::SynSeq;

const B: Dialects = Dialects::BOTH;
const V5: Dialects = Dialects::V5;
const V6: Dialects = Dialects::V6;
const E: Tol = Tol::Exact;
const COLL: &[&str] = &["pin:04-collection-edges"];

/// One r29 F row.
struct P {
    n: u16,
    ds: Dialects,
    twin: u16,
    pins: &'static [&'static str],
    title: &'static str,
    pre: &'static [&'static str],
    plots: Vec<(String, String, Tol)>,
    window: Window,
    tier: u8,
    /// `Some(code)`: an authored `refuse` (the rule-2 twins).
    refuse: Option<u16>,
    readings: Vec<Reading>,
}

fn p(
    n: u16,
    pins: &'static [&'static str],
    title: &'static str,
    pre: &'static [&'static str],
    plots: Vec<(String, String, Tol)>,
) -> P {
    P {
        n,
        ds: B,
        twin: 0,
        pins,
        title,
        pre,
        plots,
        window: Window::Head(16),
        tier: 3,
        refuse: None,
        readings: Vec::new(),
    }
}

/// The isna/val pair of each `(expr, suffix, tol)`; `kind` per value: `'s'` a string (`PROJ` =
/// `str.length`), `'b'` a bool (`val` = `E ? 1 : 0` alone), else a number.
fn pairs(vs: &[(&str, &str, char, Tol)]) -> Vec<(String, String, Tol)> {
    let mut o = Vec::new();
    for (e, k, kind, t) in vs {
        match kind {
            'b' => o.push((format!("{e} ? 1 : 0"), k.to_string(), E)),
            's' => {
                o.push((format!("na({e}) ? 1 : 0"), format!("{k}_na"), E));
                o.push((format!("na({e}) ? na : str.length({e})"), k.to_string(), E));
            }
            _ => {
                // §4.2.0's names for a single value; `<name>_na` beside a named one
                // (`pin:06-r29-pair-names`).
                let isna = if *k == "val" {
                    "isna".to_string()
                } else {
                    format!("{k}_na")
                };
                o.push((format!("na({e}) ? 1 : 0"), isna, E));
                o.push((format!("na({e}) ? na : {e}"), k.to_string(), *t));
            }
        }
    }
    o
}

/// Plain plots, `exact`.
fn plain(vs: &[(&str, &str)]) -> Vec<(String, String, Tol)> {
    vs.iter()
        .map(|(e, k)| (e.to_string(), k.to_string(), E))
        .collect()
}

fn reading(name: &str, body: ReadingBody) -> Reading {
    Reading {
        name: name.to_string(),
        body,
    }
}

/// A `head 1` reading of plain plots.
fn one(name: &str, vs: &[f64]) -> Reading {
    reading(name, ReadingBody::Rows(vec![arow(0, vs)]))
}

fn syn_steps(n: usize) -> Vec<(f64, i64)> {
    let mut q = SynSeq::new();
    (0..n)
        .map(|_| {
            let s = q.step();
            (s.syn, s.syni)
        })
        .collect()
}

/// `head n` rows of isna/val pairs, one value per `f(syn, syni)` column.
fn rows_of(n: usize, f: impl Fn(f64, i64) -> Vec<f64>) -> Vec<Row> {
    syn_steps(n)
        .iter()
        .enumerate()
        .map(|(i, &(s, si))| {
            let cells: Vec<f64> = f(s, si).into_iter().flat_map(pair).collect();
            arow(i as i64, &cells)
        })
        .collect()
}

// ---- tier 3 ------------------------------------------------------------------------------------

const CORE: &[&str] = &[
    "a@ = array.new_float(0)",
    "array.push(a@, syn)",
    "array.push(a@, syn + 1.0)",
    "array.push(a@, syn - 2.0)",
    "array.unshift(a@, 7.0)",
    "array.insert(a@, 2, 9.5)",
    "array.set(a@, 0, array.get(a@, 0) + 1.0)",
    "p@ = array.pop(a@)",
    "s@ = array.shift(a@)",
    "r@ = array.remove(a@, 1)",
    "b@ = array.from(1.0, 2.0, 3.0)",
    "c@ = array.concat(array.copy(a@), b@)",
    "sl@ = array.slice(c@, 1, 3)",
    "array.fill(b@, 4.5, 0, 2)",
    "array.reverse(c@)",
    "j@ = array.join(b@, \",\")",
];

fn core_plots() -> Vec<(String, String, Tol)> {
    let mut o = pairs(&[
        ("array.size(a@)", "size_a", 'i', E),
        ("array.first(a@)", "first_a", 'f', E),
        ("array.last(a@)", "last_a", 'f', E),
        ("p@", "pop", 'f', E),
        ("s@", "shift", 'f', E),
        ("r@", "remove", 'f', E),
        ("array.size(c@)", "size_c", 'i', E),
        ("array.get(c@, 0)", "c0", 'f', E),
        ("array.size(sl@)", "size_sl", 'i', E),
        ("array.get(sl@, 0)", "sl0", 'f', E),
        ("array.get(b@, 0)", "b0", 'f', E),
        ("array.get(b@, 2)", "b2", 'f', E),
        ("array.indexof(c@, 3.0)", "idx", 'i', E),
        ("array.lastindexof(c@, 9.5)", "lastidx", 'i', E),
        ("j@", "join", 's', E),
    ]);
    o.extend(pairs(&[("array.includes(c@, 9.5)", "includes", 'b', E)]));
    o
}

const SORT: &[&str] = &[
    "float n@ = na",
    "int ni@ = na",
    "af@ = array.from(syn, n@, syn - 5.0, syn + 3.0)",
    "df@ = array.copy(af@)",
    "ixf@ = array.sort_indices(af@, order.ascending)",
    "array.sort(af@, order.ascending)",
    "array.sort(df@, order.descending)",
    "ai@ = array.from(syni, 3, ni@, syni - 500)",
    "array.sort(ai@, order.ascending)",
    "as@ = array.from(\"bb\", str.tostring(syni), \"a\")",
    "ixs@ = array.sort_indices(as@, order.descending)",
    "array.sort(as@, order.ascending)",
];

fn sort_plots() -> Vec<(String, String, Tol)> {
    pairs(&[
        ("array.get(af@, 0)", "af0", 'f', E),
        ("array.get(af@, 1)", "af1", 'f', E),
        ("array.get(af@, 2)", "af2", 'f', E),
        ("array.get(af@, 3)", "af3", 'f', E),
        ("array.get(df@, 0)", "df0", 'f', E),
        ("array.get(df@, 3)", "df3", 'f', E),
        ("array.get(ai@, 0)", "ai0", 'i', E),
        ("array.get(ai@, 1)", "ai1", 'i', E),
        ("array.get(ai@, 2)", "ai2", 'i', E),
        ("array.get(ai@, 3)", "ai3", 'i', E),
        ("array.get(as@, 0)", "as0", 's', E),
        ("array.get(as@, 1)", "as1", 's', E),
        ("array.get(as@, 2)", "as2", 's', E),
        ("array.get(ixf@, 0)", "ixf0", 'i', E),
        ("array.get(ixf@, 3)", "ixf3", 'i', E),
        ("array.get(ixs@, 0)", "ixs0", 'i', E),
        ("array.get(ixs@, 2)", "ixs2", 'i', E),
    ])
}

/// §4.4.2's single-pass bound at the cap (`min(4·len, 64)`); `stdev` adds 2 for the root.
const MOM: Tol = Tol::Ulp(64);
const SD: Tol = Tol::Ulp(66);
/// One interpolation or one halving of a sum: two roundings.
const INTERP: Tol = Tol::Ulp(4);

const STATS_F: &[&str] = &[
    "float n@ = na",
    "x@ = array.from(syn, syn - 3.0, n@, syn + 7.5, syn - 3.0)",
    "y@ = array.from(1.0, 2.0, n@, 4.0, 5.0)",
];

fn stats_f_plots() -> Vec<(String, String, Tol)> {
    pairs(&[
        ("array.sum(x@)", "sum", 'f', Tol::Ulp(16)),
        ("array.avg(x@)", "avg", 'f', Tol::Ulp(16)),
        ("array.min(x@)", "min", 'f', E),
        ("array.max(x@)", "max", 'f', E),
        ("array.range(x@)", "range", 'f', E),
        ("array.variance(x@)", "variance", 'f', MOM),
        ("array.stdev(x@, false)", "stdev", 'f', SD),
        ("array.median(x@)", "median", 'f', INTERP),
        ("array.mode(x@)", "mode", 'f', E),
        ("array.percentile_nearest_rank(x@, 40)", "pnr", 'f', E),
        (
            "array.percentile_linear_interpolation(x@, 40)",
            "pli",
            'f',
            INTERP,
        ),
        ("array.percentrank(x@, 1)", "prank", 'f', E),
        ("array.covariance(x@, y@, true)", "cov", 'f', MOM),
    ])
}

const STATS_I: &[&str] = &[
    "int ni@ = na",
    "xi@ = array.from(syni, syni + 3, ni@, syni - 7, syni + 3)",
    "yf@ = array.from(1.0, 2.0, 3.0, 4.0, 5.0)",
];

fn stats_i_plots() -> Vec<(String, String, Tol)> {
    pairs(&[
        ("array.sum(xi@)", "sum", 'i', E),
        ("array.avg(xi@)", "avg", 'i', E),
        ("array.min(xi@)", "min", 'i', E),
        ("array.max(xi@)", "max", 'i', E),
        ("array.range(xi@)", "range", 'i', E),
        ("array.variance(xi@)", "variance", 'i', E),
        ("array.stdev(xi@)", "stdev", 'i', E),
        ("array.median(xi@)", "median", 'i', E),
        ("array.mode(xi@)", "mode", 'i', E),
        ("array.percentile_nearest_rank(xi@, 40)", "pnr", 'i', E),
        (
            "array.percentile_linear_interpolation(xi@, 40)",
            "pli",
            'i',
            E,
        ),
        ("array.percentrank(xi@, 1)", "prank", 'i', E),
        ("array.covariance(xi@, yf@, false)", "cov", 'f', MOM),
    ])
}

/// F103's two arrays: four values (`p = 25`: rank 0.5 / 0.25 / 0.75 under `C` = 1/2, 0, 1) and
/// fourteen (B096's `N`: rank 3.0 / 2.75 / 3.25 — CI-015b's separating case), shuffled.
const P4: [f64; 4] = [30.0, 10.0, 40.0, 20.0];
const P14: [f64; 14] = [
    50.0, 10.0, 140.0, 30.0, 120.0, 20.0, 90.0, 40.0, 130.0, 60.0, 110.0, 70.0, 100.0, 80.0,
];
const PCT_ARR: &[&str] = &[
    "a4@ = array.from(30.0, 10.0, 40.0, 20.0)",
    concat!(
        "a14@ = array.from(50.0, 10.0, 140.0, 30.0, 120.0, 20.0, 90.0, 40.0, 130.0, 60.0, ",
        "110.0, 70.0, 100.0, 80.0)"
    ),
];
const PCT_C: [(&str, PctC); 3] = [
    ("c_half", PctC::Half),
    ("c_zero", PctC::Zero),
    ("c_one", PctC::One),
];

fn pct_arr_readings() -> Vec<Reading> {
    PCT_C
        .iter()
        .map(|&(name, c)| one(name, &[pct_linear(&P4, 25.0, c), pct_linear(&P14, 25.0, c)]))
        .collect()
}

fn pct_ta_readings() -> Vec<Reading> {
    let x: Vec<f64> = syn_steps(64).iter().map(|s| s.0).collect();
    PCT_C
        .iter()
        .map(|&(name, c)| {
            let v = ta_pct_linear(&x, 14, 25.0, c);
            let rows = (0..64).map(|i| arow(i as i64, &pair(v[i]))).collect();
            reading(name, ReadingBody::Rows(rows))
        })
        .collect()
}

const EVERY: &[&str] = &[
    "b1@ = array.from(synb, true)",
    "b2@ = array.from(false, synb)",
    "b3@ = array.new_bool(0)",
];

/// F106: `[10, 20, 30, 40]` (no duplicates: "which occurrence" is not this pin's question).
const BS_W: [f64; 4] = [10.0, 20.0, 30.0, 40.0];
const BS_Q: [(u8, f64, &str); 8] = [
    (0, 20.0, "plain20"),
    (1, 25.0, "left25"),
    (2, 25.0, "right25"),
    (1, 5.0, "left5"),
    (2, 5.0, "right5"),
    (0, 5.0, "plain5"),
    (1, 45.0, "left45"),
    (2, 45.0, "right45"),
];
const BS_PRE: &[&str] = &[
    "wf@ = array.from(10.0, 20.0, 30.0, 40.0)",
    "wi@ = array.from(10, 20, 30, 40)",
];

fn bs_plots() -> Vec<(String, String, Tol)> {
    let f = |m: u8| match m {
        0 => "array.binary_search",
        1 => "array.binary_search_leftmost",
        _ => "array.binary_search_rightmost",
    };
    let mut o = Vec::new();
    for (arr, tag) in [("wf@", "f"), ("wi@", "i")] {
        for (m, v, k) in BS_Q {
            o.push((format!("{}({arr}, {v:?})", f(m)), format!("{tag}_{k}"), E));
        }
    }
    o
}

fn bs_readings() -> Vec<Reading> {
    [
        ("minus_one", LeftBelow::MinusOne),
        ("zero", LeftBelow::Zero),
    ]
    .iter()
    .map(|&(name, below)| {
        let one_arr: Vec<f64> = BS_Q
            .iter()
            .map(|&(m, v, _)| bsearch(&BS_W, v, m, below))
            .collect();
        let mut vs = one_arr.clone();
        vs.extend(one_arr);
        one(name, &vs)
    })
    .collect()
}

const MAP: &[&str] = &[
    "m@ = map.new<string, float>()",
    "map.put(m@, \"a\", syn)",
    "map.put(m@, \"b\", syn + 1.0)",
    "old@ = map.put(m@, \"a\", syn + 2.0)",
    "g@ = map.get(m@, \"a\")",
    "h@ = map.contains(m@, \"b\")",
    "x@ = map.get(m@, \"zz\")",
    "rm@ = map.remove(m@, \"b\")",
    "sz1@ = map.size(m@)",
    "m2@ = map.copy(m@)",
    "map.put(m2@, \"c\", 3.0)",
    "szm2@ = map.size(m2@)",
    "mm@ = map.new<string, float>()",
    "map.put(mm@, \"d\", 4.0)",
    "map.put_all(m@, mm@)",
    "k@ = map.keys(m@)",
    "v@ = map.values(m@)",
    "map.clear(m2@)",
];

fn map_plots() -> Vec<(String, String, Tol)> {
    pairs(&[
        ("old@", "old", 'f', E),
        ("g@", "get", 'f', E),
        ("h@", "contains", 'b', E),
        ("x@", "missing", 'f', E),
        ("rm@", "removed", 'f', E),
        ("sz1@", "size1", 'i', E),
        ("szm2@", "sz_copy", 'i', E),
        ("map.size(m@)", "size2", 'i', E),
        ("array.join(k@, \",\")", "keys", 's', E),
        ("array.get(v@, 1)", "value1", 'f', E),
        ("map.size(m2@)", "sz_clear", 'i', E),
    ])
}

/// F108 / F109: the 2026 UDT field rows (`sort`, `sort_indices` by field; descending by `w`).
///
/// `pin:02-bsearch-field-refused` — R-P16 (CI-052, operator 2026-10-17): TradingView refused
/// F108's first form at compile, `array.binary_search(arr_F108, 777, "k_F108")` (and the
/// `_leftmost` / `_rightmost` field forms): "The `Ob_F108` user-defined type does not include a
/// field named "#loc_42"". pine-sema now refuses a `binary_search*` field argument `3007`, so the
/// three search lines and their `bs` / `bl` / `br` plots are dropped; the re-authored F108 (sort,
/// sort_indices, descending by `w`) is pending its capture.
const UDT_ROWS: &[&str] = &[
    "type Ob@",
    ">int k@ = 0",
    ">int id@ = 0",
    ">float w@ = 0.0",
    "arr@ = array.new<Ob@>()",
    "array.push(arr@, Ob@.new(2 * syni, 1, syn))",
    "array.push(arr@, Ob@.new(2001, 2, 1.5))",
    "array.push(arr@, Ob@.new(-1, 3, 2500.0))",
    "array.push(arr@, Ob@.new(777, 4, syn - 1000.0))",
    "ix@ = array.sort_indices(arr@, order.descending, \"k@\")",
    "array.sort(arr@, order.ascending, \"k@\")",
    "o0@ = array.get(arr@, 0)",
    "o1@ = array.get(arr@, 1)",
    "o2@ = array.get(arr@, 2)",
    "o3@ = array.get(arr@, 3)",
    "array.sort(arr@, order.descending, \"w@\")",
    "ow@ = array.get(arr@, 0)",
];

fn udt_plots() -> Vec<(String, String, Tol)> {
    pairs(&[
        ("o0@.id@", "id0", 'i', E),
        ("o1@.id@", "id1", 'i', E),
        ("o2@.id@", "id2", 'i', E),
        ("o3@.id@", "id3", 'i', E),
        ("array.get(ix@, 0)", "ix0", 'i', E),
        ("array.get(ix@, 3)", "ix3", 'i', E),
        ("ow@.id@", "wid0", 'i', E),
    ])
}

/// F110 / F111: equal keys told apart by `id` (`pin:04-udt-sort-ties`).
const TIE_K: [f64; 6] = [2.0, 1.0, 2.0, 1.0, 2.0, 1.0];
const TIES: &[&str] = &[
    "type Ob@",
    ">int k@ = 0",
    ">int id@ = 0",
    "a@ = array.new<Ob@>()",
    "array.push(a@, Ob@.new(2, 1))",
    "array.push(a@, Ob@.new(1, 2))",
    "array.push(a@, Ob@.new(2, 3))",
    "array.push(a@, Ob@.new(1, 4))",
    "array.push(a@, Ob@.new(2, 5))",
    "array.push(a@, Ob@.new(1, 6))",
    "d@ = array.copy(a@)",
    "array.sort(a@, order.ascending, \"k@\")",
    "array.sort(d@, order.descending, \"k@\")",
];

fn tie_plots() -> Vec<(String, String, Tol)> {
    let mut o = Vec::new();
    for (arr, tag) in [("a@", "asc"), ("d@", "desc")] {
        for i in 0..TIE_K.len() {
            o.push((format!("array.get({arr}, {i}).id@"), format!("{tag}{i}"), E));
        }
    }
    o
}

fn tie_readings() -> Vec<Reading> {
    let ids = |perm: Vec<usize>| {
        perm.into_iter()
            .map(|i| (i + 1) as f64)
            .collect::<Vec<f64>>()
    };
    let row = |f: fn(&[f64], bool) -> Vec<usize>| {
        let mut v = ids(f(&TIE_K, false));
        v.extend(ids(f(&TIE_K, true)));
        v
    };
    vec![
        one("stable", &row(stable_sort_order)),
        one("heap_order", &row(heap_sort_order)),
    ]
}

/// F112 / F113: an `na` element in a field sort (`pin:04-udt-sort-na-object`). A field of an `na`
/// object is read only inside an `if` (a ternary arm might be evaluated, `pin:U14`).
const NA_OBJ: &[&str] = &[
    "type Ob@",
    ">int k@ = 0",
    ">int id@ = 0",
    "Ob@ n@ = na",
    "a@ = array.new<Ob@>()",
    "array.push(a@, Ob@.new(5, 1))",
    "array.push(a@, n@)",
    "array.push(a@, Ob@.new(2, 3))",
    "array.sort(a@, order.ascending, \"k@\")",
    "int i0@ = -1",
    "int i1@ = -1",
    "int i2@ = -1",
    "o0@ = array.get(a@, 0)",
    "o1@ = array.get(a@, 1)",
    "o2@ = array.get(a@, 2)",
    "if not na(o0@)",
    ">i0@ := o0@.id@",
    "if not na(o1@)",
    ">i1@ := o1@.id@",
    "if not na(o2@)",
    ">i2@ := o2@.id@",
];

fn na_obj_readings() -> Vec<Reading> {
    vec![
        reading(
            "runtime_fault",
            ReadingBody::Fault {
                kind: FaultKind::ScriptRefused,
                row: 0,
            },
        ),
        one("na_last", &[3.0, 1.0, -1.0]),
        one("na_first", &[-1.0, 3.0, 1.0]),
    ]
}

const METHOD: &[&str] = &[
    "a@ = array.new_float(0)",
    "a@.push(syn)",
    "a@.push(syn - 1.0)",
    "a@.unshift(syn + 2.0)",
    "b@ = array.new_float(0)",
    "array.push(b@, syn)",
    "array.push(b@, syn - 1.0)",
    "array.unshift(b@, syn + 2.0)",
    "a@.sort()",
    "array.sort(b@)",
    "m@ = map.new<string, float>()",
    "m@.put(\"k\", syn)",
    "n@ = map.new<string, float>()",
    "map.put(n@, \"k\", syn)",
];

fn method_plots() -> Vec<(String, String, Tol)> {
    pairs(&[
        ("a@.get(0)", "m_get0", 'f', E),
        ("array.get(b@, 0)", "f_get0", 'f', E),
        ("a@.last()", "m_last", 'f', E),
        ("array.last(b@)", "f_last", 'f', E),
        ("a@.size()", "m_size", 'i', E),
        ("array.size(b@)", "f_size", 'i', E),
        ("m@.get(\"k\")", "m_mget", 'f', E),
        ("map.get(n@, \"k\")", "f_mget", 'f', E),
    ])
}

// ---- open issues -------------------------------------------------------------------------------

const IMOD: [(&str, i64, bool); 3] = [
    // (expr, divisor, negate the dividend)
    ("syni % (0 - 7)", -7, false),
    ("(0 - syni) % (0 - 7)", -7, true),
    ("(0 - syni) % 7", 7, true),
];

/// F116: dividends `±(syni + 0.25)` never a multiple of `2.5` (no zero remainder, so the sign of
/// zero cannot confound), every intermediate exact in binary64 (so `pin:03-float-mod`'s two forms
/// agree and only the sign rule is asked).
const FMOD: [(&str, f64, bool); 4] = [
    ("(syni + 0.25) % (0.0 - 2.5)", -2.5, false),
    ("(-0.25 - syni) % 2.5", 2.5, true),
    ("(-0.25 - syni) % (0.0 - 2.5)", -2.5, true),
    ("(syni + 0.25) % 2.5", 2.5, false),
];

fn mod_readings() -> (Vec<Reading>, Vec<Reading>) {
    let int = |f: fn(i64, i64) -> Option<i64>| {
        rows_of(64, |_, si| {
            IMOD.iter()
                .map(|&(_, b, neg)| {
                    let a = if neg { -si } else { si };
                    f(a, b).map_or(NA, |v| v as f64)
                })
                .collect()
        })
    };
    let flt = |f: fn(f64, f64) -> f64| {
        rows_of(64, |_, si| {
            FMOD.iter()
                .map(|&(_, b, neg)| {
                    let a = if neg {
                        -0.25 - si as f64
                    } else {
                        si as f64 + 0.25
                    };
                    f(a, b)
                })
                .collect()
        })
    };
    (
        vec![
            reading("floored", ReadingBody::Rows(int(mod_floored))),
            reading("truncated", ReadingBody::Rows(int(mod_truncated))),
        ],
        vec![
            reading("floored", ReadingBody::Rows(flt(fmod_exact))),
            reading("truncated", ReadingBody::Rows(flt(|a, b| a % b))),
        ],
    )
}

const ASSOC: [(&str, &str); 3] = [
    ("(0.1 + 0.2) + (syn - syn)", "grouped"),
    ("0.1 + (0.2 + (syn - syn))", "inner"),
    ("0.1 + 0.2 + syn - syn", "flat"),
];

/// Titles too long for one line.
const T099: &str = concat!(
    "array core: new push get set pop shift unshift insert remove first last slice concat fill ",
    "reverse includes indexof lastindexof join"
);
const T100: &str =
    "array.sort and sort_indices: float, int, string; ascending, descending; na placement";
const T106: &str =
    "binary_search, _leftmost, _rightmost on float and int, a value below the first element";
const T107: &str = "map: put get contains remove clear size keys values copy put_all";
const T114: &str = "method form next to the function form, the same values";
const T117: &str = concat!(
    "F019's sum grouped three ways: (0.1 + 0.2) + (syn - syn), 0.1 + (0.2 + (syn - syn)), ",
    "0.1 + 0.2 + syn - syn"
);

fn rows() -> Vec<P> {
    let (imod, fmod) = mod_readings();
    let mut o = vec![
        p(99, COLL, T099, CORE, core_plots()),
        p(100, COLL, T100, SORT, sort_plots()),
        p(
            101,
            COLL,
            "array statistics of a float array with an na element",
            STATS_F,
            stats_f_plots(),
        ),
        p(
            102,
            COLL,
            "array statistics of an int array with an na element",
            STATS_I,
            stats_i_plots(),
        ),
        P {
            window: Window::Head(1),
            readings: pct_arr_readings(),
            ..p(
                103,
                &["pin:04-percentile-linear-c-half"],
                "array.percentile_linear_interpolation at p = 25, N = 4 and N = 14",
                PCT_ARR,
                plain(&[
                    ("array.percentile_linear_interpolation(a4@, 25)", "n4"),
                    ("array.percentile_linear_interpolation(a14@, 25)", "n14"),
                ]),
            )
        },
        P {
            window: Window::Head(64),
            tier: 1,
            readings: pct_ta_readings(),
            ..p(
                104,
                &["pin:04-percentile-linear-c-half"],
                "ta.percentile_linear_interpolation(syn, 14, 25)",
                &["e@ = ta.percentile_linear_interpolation(syn, 14, 25)"],
                pairs(&[("e@", "val", 'f', E)]),
            )
        },
        p(
            105,
            COLL,
            "array.every and array.some, an empty array included",
            EVERY,
            pairs(&[
                ("array.every(b1@)", "every1", 'b', E),
                ("array.some(b2@)", "some2", 'b', E),
                ("array.every(b3@)", "every0", 'b', E),
                ("array.some(b3@)", "some0", 'b', E),
            ]),
        ),
        P {
            window: Window::Head(1),
            readings: bs_readings(),
            ..p(106, &["pin:04-binary-search"], T106, BS_PRE, bs_plots())
        },
        p(107, COLL, T107, MAP, map_plots()),
    ];
    // The UDT field rows (v6) and their rule-2 twins (v5, `refuse 3005`).
    let udt: [(
        u16,
        &'static [&'static str],
        &'static str,
        &'static [&'static str],
    ); 3] = [
        (
            108,
            COLL,
            "array.sort and sort_indices by a UDT field",
            UDT_ROWS,
        ),
        (
            110,
            &["pin:04-udt-sort-ties"],
            "array.sort by a UDT field: ties told apart by another field",
            TIES,
        ),
        (
            112,
            &["pin:04-udt-sort-na-object"],
            "array.sort by a UDT field with an na element",
            NA_OBJ,
        ),
    ];
    for (n, pins, title, pre) in udt {
        let (plots, window, readings) = match n {
            108 => (udt_plots(), Window::Head(16), Vec::new()),
            110 => (tie_plots(), Window::Head(1), tie_readings()),
            _ => (
                plain(&[("i0@", "id0"), ("i1@", "id1"), ("i2@", "id2")]),
                Window::Head(1),
                na_obj_readings(),
            ),
        };
        o.push(P {
            ds: V6,
            twin: n + 1,
            window,
            readings,
            ..p(n, pins, title, pre, plots.clone())
        });
        o.push(P {
            ds: V5,
            twin: n,
            window,
            refuse: Some(3005),
            ..p(n + 1, &[], title, pre, plots)
        });
    }
    o.extend([
        p(114, COLL, T114, METHOD, method_plots()),
        P {
            window: Window::Head(64),
            tier: 1,
            readings: imod,
            ..p(
                115,
                &["pin:06-mod-sign"],
                "int % with a negative divisor, a negative dividend, both",
                &[],
                pairs(&IMOD.map(|(e, _, _)| (e, "", 'i', E))),
            )
        },
        P {
            window: Window::Head(64),
            tier: 1,
            readings: fmod,
            ..p(
                116,
                &["pin:06-mod-sign"],
                "float % with a negative divisor, a negative dividend, both; no zero remainder",
                &[],
                pairs(&FMOD.map(|(e, _, _)| (e, "", 'f', E))),
            )
        },
        P {
            window: Window::Head(64),
            tier: 1,
            ..p(
                117,
                &[],
                T117,
                &[],
                pairs(&ASSOC.map(|(e, k)| (e, k, 'f', E))),
            )
        },
        P {
            window: Window::Head(1),
            tier: 1,
            readings: vec![
                reading(
                    "refuse_literal",
                    ReadingBody::Refuse(super::readings::R_P2_REFUSE),
                ),
                one("na", &pair(NA)),
                one("ieee", &pair(f64::INFINITY)),
            ],
            ..p(
                118,
                &["pin:U15-float-div-zero"],
                "syn / (2 - 2): a constant-expression zero divisor",
                &[],
                pairs(&[("syn / (2 - 2)", "val", 'f', E)]),
            )
        },
        P {
            window: Window::Head(1),
            tier: 1,
            readings: vec![
                reading("refuse_dynamic", ReadingBody::Refuse(DiagCode(3008))),
                reading(
                    "runtime_fault",
                    ReadingBody::Fault {
                        kind: FaultKind::ScriptRefused,
                        row: 0,
                    },
                ),
                one("na_default", &[11.0]),
                one("no_loop", &[0.0]),
                // R-P17 (CI-053): TradingView runs the body once per bar (cnt = 1 on all
                // 24 493 bars, v5 and v6) — the first pass runs, `i + na` is na, the bound test
                // fails.
                one("one_pass", &[1.0]),
            ],
            ..p(
                119,
                &["pin:02-for-step"],
                "for with an na series step",
                &[
                    "int s@ = syni < 0 ? 1 : na",
                    "cnt@ = 0",
                    "for i@ = 0 to 10 by s@",
                    ">cnt@ += 1",
                ],
                plain(&[("cnt@", "val")]),
            )
        },
    ]);
    o
}

fn line(s: &str, id: FixtureId) -> Line {
    let sfx = format!("_{}", id.as_str());
    match s.strip_prefix('>') {
        Some(rest) => Line::block(rest.replace('@', &sfx)),
        None => Line::top(s.replace('@', &sfx)),
    }
}

fn cell_of(r: P) -> Cell {
    let id = FixtureId::from_parts(Group::F, r.n);
    let sfx = format!("_{}", id.as_str());
    // A value without a suffix (F115–F117's `pairs` of bare expressions) is numbered `v<i>`.
    let mut plots = Vec::new();
    let mut tols = Vec::new();
    let mut k = 0;
    for (e, s, t) in r.plots {
        let suffix = if s.is_empty() || s == "_na" {
            if s.is_empty() {
                k += 1;
                format!("val{k}")
            } else {
                format!("isna{}", k + 1)
            }
        } else {
            s
        };
        plots.push((e.replace('@', &sfx), suffix));
        tols.push(t);
    }
    let (expect, oracle) = match r.refuse {
        Some(c) => (Expect::Refuse { code: DiagCode(c) }, Oracle::Authored),
        None => (Expect::Pending, Oracle::Tv),
    };
    let pins: Vec<String> = r.pins.iter().map(|p| p.to_string()).collect();
    let twin = (r.twin != 0).then(|| FixtureId::from_parts(Group::F, r.twin));
    let script = Script {
        id,
        dialects: r.ds,
        about: vec![r.title.to_string()],
        twin,
        pins: pins.clone(),
        decl: None,
        prelude: Prelude::Syn1,
        body: r.pre.iter().map(|s| line(s, id)).collect(),
        plots,
    };
    Cell {
        script,
        title: r.title.to_string(),
        pins,
        oracle,
        tier: r.tier,
        window: r.window,
        tols,
        expect,
        readings: r.readings,
        run: Run::default(),
    }
}

/// F099–F119, in id order.
pub fn cells() -> Vec<Cell> {
    rows().into_iter().map(cell_of).collect()
}
