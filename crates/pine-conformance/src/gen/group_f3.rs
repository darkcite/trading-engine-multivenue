// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Group F, r34 block (F120–F128): the probe sheet r34 (vault `capture/probes-r34-2026-10-18.md`,
//! operator ruling R-P26), appended after F119 so no captured id moves (I6). GENERATOR-TIME ONLY.
//!
//! COPY-DOCTRINE: generator-time reading rows; nothing here runs on a bar step.
//!
//! Each script is the sheet's block in the house style (§4.2.0: SYN-1, identifiers suffixed
//! `_<ID>`, one plain plot per column, named as the sheet names it). Readings (§4.9.4) are the
//! sheet's candidate readings, computed by the Rust twins of `gen/readings.rs`; where a sheet cell
//! offers alternatives the reading takes the decided rule or the alternative the sheet lists, and a
//! reading the sheet leaves open is not authored (F125: CI-022 has no pin, as F117).
//!
//! Pins this file takes:
//! * `pin:06-r34-bar-reads` — F120, F121, F123 and F125 read `bar_index` / `close` exactly as the
//!   sheet writes them (a constant made a series without arithmetic; a series zero no fold sees).
//!   A `Syn` run steps `SynBars` (`bar_index` is the step, `close` one constant), so every column
//!   but F125's `close` reads the same on both sides; I5's scan lists the four ids.
//! * `pin:06-r34-inf-export` — a reading cell for `±inf` is written `na`: `capture/format.tsv`
//!   (probe-00) prints `na`, `+inf` and `-inf` as one empty cell (F121 `r_fold`'s `nz`).
//! * `pin:06-f128-sum-tol` — F128's `val` compares at `ulp 12`, §4.2.2's `ta.sma` bound at length 3
//!   (`pin:06-twin-sum-order`): F128 asks the state law; the sum's arithmetic is F127's question.
//! * `pin:06-r34-mintick` — F123's readings take the capture chart's mintick, `0.1`
//!   (`tests/conformance/syminfo.tsv` `mintick_1e6 100000`, the sheet's P4).

use super::emit::{Line, Prelude, Script};
use super::readings::{
    acc_sum, eq_tol, ge_tol, gt_tol, mintick_band, mintick_f64, mintick_int, neumaier_sum, pair,
    per_call_runs, percentrank, plain_sum, restart_runs, row as arow, sig15_away, sig15_even,
    RankDen, RollSum, NA,
};
use super::{Cell, Run};
use crate::fixture::{
    Dialects, Expect, FixtureId, Group, Oracle, Reading, ReadingBody, Row, Tol, Window,
};
use crate::syn::SynSeq;

const E: Tol = Tol::Exact;

/// One r34 F row.
struct P {
    n: u16,
    pin: Option<&'static str>,
    about: &'static [&'static str],
    pre: &'static [&'static str],
    plots: &'static [(&'static str, &'static str)],
    /// The tolerance of plot `k` where it is not `exact`.
    tol: Option<(usize, Tol)>,
    window: Window,
    tier: u8,
    readings: Vec<Reading>,
}

fn reading(name: &str, rows: Vec<Row>) -> Reading {
    Reading {
        name: name.to_string(),
        body: ReadingBody::Rows(rows),
    }
}

/// `n` rows of the same cells.
fn same_rows(name: &str, n: usize, cells: &[f64]) -> Reading {
    reading(name, (0..n).map(|i| arow(i as i64, cells)).collect())
}

fn b(c: bool) -> f64 {
    if c {
        1.0
    } else {
        0.0
    }
}

fn syn_n(n: usize) -> Vec<f64> {
    let mut q = SynSeq::new();
    (0..n).map(|_| q.step().syn).collect()
}

// ---- P1 · F120: int width (CAR-R33-5) ----------------------------------------------------------

const F120_PRE: &[&str] = &[
    "big@  = bar_index >= 0 ? 9007199254740992 : 0",
    "odd@  = bar_index >= 0 ? 9007199254740993 : 0",
    "imax@ = bar_index >= 0 ? 9223372036854775807 : 0",
    "sq@   = bar_index >= 0 ? 3037000500 : 0",
    "huge@ = bar_index >= 0 ? 1e30 : 0.0",
    "b1@   = big@ + 1",
];
const F120_PLOTS: &[(&str, &str)] = &[
    ("b1@ == big@ ? 1 : 0", "eqadd"),
    ("odd@ == big@ ? 1 : 0", "eqlit"),
    ("b1@ % 2", "mod"),
    ("imax@ + 1 > 0 ? 1 : 0", "wrap"),
    ("(imax@ * 2) / imax@", "mul"),
    ("sq@ * sq@", "sq"),
    ("na(int(huge@)) ? 1 : 0", "castna"),
    ("int(huge@)", "cast"),
    ("b1@ - big@", "sub"),
];
const BIG: i64 = 9_007_199_254_740_992;
const ODD: i64 = 9_007_199_254_740_993;
const IMAX: i64 = i64::MAX;
const SQ: i64 = 3_037_000_500;
const HUGE: f64 = 1e30;

/// `pin:06-int-width`'s readings of F120's nine columns (the sheet's P1 table): `binary64`
/// (CAR-R33-5: binary64 values, `int()` an `F_TRUNC` with no range check), `i64_wrapping` (ours:
/// the `I_*` lanes; `int()` out of range is `na`), `i64_saturating`. A series int `/` divides as
/// floats (W1); a plotted int is its binary64.
fn f120_readings() -> Vec<Reading> {
    let f = |v: i64| v as f64;
    let (big, odd, imax, sq) = (f(BIG), f(ODD), f(IMAX), f(SQ));
    let b1 = big + 1.0;
    let binary64 = [
        b(b1 == big),
        b(odd == big),
        b1 % 2.0,
        b(imax + 1.0 > 0.0),
        (imax * 2.0) / imax,
        sq * sq,
        b(HUGE.trunc().is_nan()),
        HUGE.trunc(),
        b1 - big,
    ];
    let w1 = BIG.wrapping_add(1);
    let wrapping = [
        b(w1 == BIG),
        b(ODD == BIG),
        f(w1.wrapping_rem(2)),
        b(IMAX.wrapping_add(1) > 0),
        f(IMAX.wrapping_mul(2)) / f(IMAX),
        f(SQ.wrapping_mul(SQ)),
        1.0,
        NA,
        f(w1.wrapping_sub(BIG)),
    ];
    let s1 = BIG.saturating_add(1);
    let saturating = [
        b(s1 == BIG),
        b(ODD == BIG),
        f(s1 % 2),
        b(IMAX.saturating_add(1) > 0),
        f(IMAX.saturating_mul(2)) / f(IMAX),
        f(SQ.saturating_mul(SQ)),
        0.0,
        f(HUGE as i64),
        f(s1.saturating_sub(BIG)),
    ];
    vec![
        same_rows("binary64", 64, &binary64),
        same_rows("i64_wrapping", 64, &wrapping),
        same_rows("i64_saturating", 64, &saturating),
    ]
}

// ---- P2 · F121: where ±inf becomes na (CAR-R33-6) ----------------------------------------------

const F121_PRE: &[&str] = &["lg@ = math.log(close - close)", "ex@ = syn * 1e303 * syn"];
const F121_PLOTS: &[(&str, &str)] = &[
    ("lg@ < 0 ? 1 : 0", "lt"),
    ("na(lg@) ? 1 : 0", "lgna"),
    ("ex@ > 0 ? 1 : 0", "gt"),
    ("na(ex@) ? 1 : 0", "exna"),
    ("nz(ex@, 7)", "nz"),
    ("ex@ - ex@", "sub"),
];

/// CAR-R33-6's three readings.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NonFinite {
    /// Every float operation's `±inf` result is `na`.
    Op,
    /// `±inf` survives; `na()` and `nz` see it as `na`.
    Na,
    /// Only a folded constant turns non-finite into `na` (ours today).
    Fold,
}

/// F121's six columns on one row of `syn` under `r` (`close − close` is `0`: both sides).
fn f121_row(syn: f64, r: NonFinite) -> [f64; 6] {
    let op = |v: f64| {
        if r == NonFinite::Op && v.is_infinite() {
            NA
        } else {
            v
        }
    };
    let isna = |v: f64| v.is_nan() || (r == NonFinite::Na && v.is_infinite());
    let lg = op(0.0f64.ln());
    let ex = op(op(syn * 1e303) * syn);
    let nz = if isna(ex) { 7.0 } else { ex };
    // `ex - ex` in IEEE: 0 for a finite value, NaN for ±inf or na.
    let sub = if ex.is_finite() { 0.0 } else { NA };
    // pin:06-r34-inf-export: an infinite plot value exports as an empty cell, read `na`.
    let export = |v: f64| if v.is_infinite() { NA } else { v };
    [
        b(lg < 0.0),
        b(isna(lg)),
        b(ex > 0.0),
        b(isna(ex)),
        export(nz),
        export(op(sub)),
    ]
}

fn f121_readings() -> Vec<Reading> {
    let x = syn_n(64);
    [
        ("r_op", NonFinite::Op),
        ("r_na", NonFinite::Na),
        ("r_fold", NonFinite::Fold),
    ]
    .iter()
    .map(|&(name, r)| {
        let rows = (0..64)
            .map(|i| arow(i as i64, &f121_row(x[i], r)))
            .collect();
        reading(name, rows)
    })
    .collect()
}

// ---- P3 · F122: float == on series (CAR-R33-7) -------------------------------------------------

const F122_PRE: &[&str] = &["a@  = syn + 5e-11", "a2@ = syn + 2e-10", "bg@ = syn * 1e7"];
const F122_PLOTS: &[(&str, &str)] = &[
    ("a@ == syn ? 1 : 0", "eq"),
    ("a@ != syn ? 1 : 0", "ne"),
    ("a@ > syn ? 1 : 0", "gt"),
    ("syn < a@ ? 1 : 0", "lt"),
    ("a@ >= syn and syn >= a@ ? 1 : 0", "both"),
    ("a2@ == syn ? 1 : 0", "eq2"),
    ("bg@ + 1e-4 == bg@ ? 1 : 0", "big"),
    ("a@ - syn", "diff"),
];

/// `pin:U06-float-eq-v6`'s readings over F122 (the sheet's P3): `exact` (ours), `abs_1e10`
/// (CAR-R33-7's name: the tolerance on `==` / `!=` only) and `abs_1e10_all` (the tolerance on
/// every comparison, `a > b` iff `a − b > 1e-10`); `diff` is IEEE `(syn + 5e-11) − syn` in all.
fn f122_readings() -> Vec<Reading> {
    let x = syn_n(64);
    let row = |s: f64, eq_t: bool, ord_t: bool| {
        let (a, a2, bg) = (s + 5e-11, s + 2e-10, s * 1e7);
        let eq = |p: f64, q: f64| if eq_t { eq_tol(p, q) } else { p == q };
        let gt = |p: f64, q: f64| if ord_t { gt_tol(p, q) } else { p > q };
        let ge = |p: f64, q: f64| if ord_t { ge_tol(p, q) } else { p >= q };
        [
            b(eq(a, s)),
            b(!eq(a, s)),
            b(gt(a, s)),
            b(gt(a, s)),
            b(ge(a, s) && ge(s, a)),
            b(eq(a2, s)),
            b(eq(bg + 1e-4, bg)),
            a - s,
        ]
    };
    [
        ("exact", false, false),
        ("abs_1e10", true, false),
        ("abs_1e10_all", true, true),
    ]
    .iter()
    .map(|&(name, e, o)| {
        let rows = (0..64).map(|i| arow(i as i64, &row(x[i], e, o))).collect();
        reading(name, rows)
    })
    .collect()
}

// ---- P4 · F123: math.round_to_mintick near a decimal tie ---------------------------------------

const F123_PRE: &[&str] = &["k@(x@) => bar_index >= 0 ? x@ : na"];
const F123_PLOTS: &[(&str, &str)] = &[
    ("math.round_to_mintick(k@(76050.0) * 1.005)", "e14"),
    ("math.round_to_mintick(k@(77110.0) * 0.995)", "e46"),
    ("math.round_to_mintick(k@(77410.0) * 1.005)", "e77"),
    ("math.round_to_mintick(k@(63490.0) * 1.005)", "e161"),
    ("math.round_to_mintick(k@(63750.0) * 1.005)", "e219"),
    ("math.round_to_mintick(k@(77670.0) * 1.005)", "e275"),
    ("math.round_to_mintick(k@(76065.0) * 1.01)", "e99"),
    ("math.round_to_mintick(k@(76430.25))", "tie"),
    ("math.round_to_mintick(k@(-76430.25))", "ntie"),
    ("math.round_to_mintick(k@(1000.05))", "n0"),
    ("math.round_to_mintick(k@(1000.049999999995))", "n1"),
    ("math.round_to_mintick(k@(1000.049999999991))", "n2"),
    ("math.round_to_mintick(k@(1000.049999999989))", "n3"),
    ("math.round_to_mintick(k@(1000.04999999998))", "n4"),
];

/// `pin:06-r34-mintick`: the capture chart's `syminfo.mintick`, ×1e6.
const F123_MINTICK_1E6: i64 = 100_000;
/// R-band's edge: `1e-10` ticks (CAR-R33-7's constant).
const BAND_DEN: u128 = 10_000_000_000;

/// F123's fourteen inputs, formed as the script forms them (`k(a) * b`, binary64).
fn f123_inputs() -> [f64; 14] {
    [
        76050.0 * 1.005,
        77110.0 * 0.995,
        77410.0 * 1.005,
        63490.0 * 1.005,
        63750.0 * 1.005,
        77670.0 * 1.005,
        76065.0 * 1.01,
        76430.25,
        -76430.25,
        1000.05,
        1000.049999999995,
        1000.049999999991,
        1000.049999999989,
        1000.04999999998,
    ]
}

/// `pin:U04-round-ties`' readings of `math.round_to_mintick` (the sheet's P4): `r_int` (ours:
/// `to_1e6` then an integer tie, U04 `half_away` — decided, CAR-R33-8), `r_f64` (`round(x /
/// mintick) · mintick`, half-up on the f64 quotient, the product as written) and `r_band` (up iff
/// the exact distance below the decimal tie is ≤ 1e-10 ticks, else nearest; an exact tie under
/// U04's decided `half_away`; `pin:06-r34-band-formation`).
fn f123_readings() -> Vec<Reading> {
    let xs = f123_inputs();
    let mintick = F123_MINTICK_1E6 as f64 / 1e6;
    let r_int = xs.map(|x| mintick_int(x, F123_MINTICK_1E6, false));
    let r_f64 = xs.map(|x| mintick_f64(x, mintick));
    let r_band = xs.map(|x| mintick_band(x, F123_MINTICK_1E6, BAND_DEN));
    vec![
        same_rows("r_int", 16, &r_int),
        same_rows("r_f64", 16, &r_f64),
        same_rows("r_band", 16, &r_band),
    ]
}

// ---- P5 · F124: constant-fold precision (CAR-R33-4) --------------------------------------------

const F124_PLOTS: &[(&str, &str)] = &[
    ("0.1 + 0.2", "add"),
    ("0.3 - 0.1", "sub"),
    ("0.1 * 3.0", "mul"),
    ("1.0 / 3.0", "div"),
    ("2.0 / 3.0", "div2"),
    ("1.0000000000000002", "lit"),
    ("1234567890123.125 % 1e15", "tie"),
    ("0.1 + 0.2 == 0.3 ? 1 : 0", "eq"),
];

/// `pin:02-fold-sig15-ties`' readings (the sheet's P5): `ieee_fold` (folds as written, ours
/// except `%`; the folded `%` rounded by sema's `sig15`, a tie to even — 1234567890123.12),
/// `sig15_fold` (every fold rounded to 15 significant digits, literals exempt, the tie half away)
/// and `sig15_fold_literals` (literals rounded too). `eq` is the control: 1 under every reading
/// (TradingView's U06 tolerance, F061 F062). Not authored: a `%` fold left unrounded
/// (1234567890123.125) — F098 already shows the folded `%` rounded.
fn f124_readings() -> Vec<Reading> {
    let lit = 1.0 + f64::EPSILON;
    let tie = 1_234_567_890_123.125f64 % 1e15;
    let ieee = [
        0.1 + 0.2,
        0.3 - 0.1,
        0.1 * 3.0,
        1.0 / 3.0,
        2.0 / 3.0,
        lit,
        sig15_even(tie),
        b(eq_tol(0.1 + 0.2, 0.3)),
    ];
    let sig = |lit_v: f64| {
        [
            sig15_away(0.1 + 0.2),
            sig15_away(0.3 - 0.1),
            sig15_away(0.1 * 3.0),
            sig15_away(1.0 / 3.0),
            sig15_away(2.0 / 3.0),
            lit_v,
            sig15_away(tie),
            b(eq_tol(sig15_away(0.1 + 0.2), 0.3)),
        ]
    };
    vec![
        same_rows("ieee_fold", 4, &ieee),
        same_rows("sig15_fold", 4, &sig(lit)),
        same_rows("sig15_fold_literals", 4, &sig(sig15_away(lit))),
    ]
}

// ---- P6 · F125: the re-association law (CI-022) ------------------------------------------------

const F125_PRE: &[&str] = &["z@ = syn - syn"];
const F125_PLOTS: &[(&str, &str)] = &[
    ("0.1 + (syn - syn)", "c1"),
    ("(syn - syn) + 0.1", "c1r"),
    ("0.2 + 0.1 + (syn - syn)", "c2r"),
    ("0.1 + 0.2 + z@", "var"),
    ("(0.1 + 0.2) * (syn / syn)", "mul"),
    ("0.1 + 0.2 + (close - close)", "close"),
    ("0.5 + (syn - syn)", "half"),
    ("math.max(0.1 + (syn - syn), 0.0)", "call"),
];

// ---- P7 · F126: array.percentrank's denominator ------------------------------------------------

const F126_PRE: &[&str] = &[
    "a@ = array.from(10.0, 20.0, 30.0, 40.0)",
    "b@ = array.from(10.0, na, 30.0, 40.0)",
    "c@ = array.from(10, 20, 30, 40)",
];
const F126_PLOTS: &[(&str, &str)] = &[
    ("array.percentrank(a@, 3)", "top"),
    ("array.percentrank(a@, 1)", "second"),
    ("array.percentrank(b@, 3)", "natop"),
    ("array.percentrank(c@, 3)", "int"),
];

/// `pin:04-array-percentrank`'s two surviving readings (the sheet's P7): `size_minus_one` (ours)
/// and `non_na_count`; the int row truncates toward zero (`pin:04-array-int-stats-provisional`).
fn f126_readings() -> Vec<Reading> {
    let a = [10.0, 20.0, 30.0, 40.0];
    let bb = [10.0, NA, 30.0, 40.0];
    [
        ("size_minus_one", RankDen::SizeMinusOne),
        ("non_na_count", RankDen::NonNaCount),
    ]
    .iter()
    .map(|&(name, d)| {
        let cells = [
            percentrank(&a, 3, d),
            percentrank(&a, 1, d),
            percentrank(&bb, 3, d),
            percentrank(&a, 3, d).trunc(),
        ];
        same_rows(name, 1, &cells)
    })
    .collect()
}

// ---- P14 · F127: math.sum, rolling or fresh ----------------------------------------------------

/// `pin:04-cog-tv-order`'s readings of `math.sum(syn, 14)` over `head 32` (the sheet's P14):
/// `rolling` (ours: pine-std's window, `readings::RollSum`), `fresh_plain` and `fresh_neumaier` (a
/// fresh pass each bar, oldest first). The sheet names "fresh (plain or Neumaier)" as one reading
/// — they agree through row 20 and part on rows 21, 22 and 25, so both are authored.
fn f127_readings() -> Vec<Reading> {
    let x = syn_n(32);
    let mut w = RollSum::new(14);
    let rolling: Vec<f64> = x.iter().map(|&v| w.push(v)).collect();
    let fresh = |f: fn(&[f64]) -> f64| -> Vec<f64> {
        (0..32)
            .map(|i| if i < 13 { NA } else { f(&x[i - 13..=i]) })
            .collect()
    };
    let pairs =
        |v: &[f64]| -> Vec<Row> { (0..v.len()).map(|i| arow(i as i64, &pair(v[i]))).collect() };
    vec![
        reading("rolling", pairs(&rolling)),
        reading("fresh_plain", pairs(&fresh(plain_sum))),
        reading("fresh_neumaier", pairs(&fresh(neumaier_sum))),
    ]
}

// ---- P15 · F128: which in-loop stateful calls restart ------------------------------------------

/// `pin:02-ta-in-loop-restart-scope`'s readings of `acc += math.sum(syn, 3)` three times a bar
/// over `head 16` (the sheet's P15): `per_call` (ours for `math.*`: one window fed every
/// execution) and `restart` (as `ta.*` since CAR-R33-9: every execution from the committed
/// window), both over pine-std's rolling window (`pin:06-f128-sum-tol`).
fn f128_readings() -> Vec<Reading> {
    let x = syn_n(16);
    let per_call = per_call_runs(RollSum::new(3), 16, 3, |w, t, _| w.push(x[t]));
    let restart = restart_runs(RollSum::new(3), 16, 3, |w, t, _| w.push(x[t]));
    let rows = |runs: &[Vec<f64>]| -> Vec<Row> {
        runs.iter()
            .enumerate()
            .map(|(i, r)| arow(i as i64, &pair(acc_sum(r))))
            .collect()
    };
    vec![
        reading("per_call", rows(&per_call)),
        reading("restart", rows(&restart)),
    ]
}

fn rows() -> Vec<P> {
    let p = |n: u16,
             pin: Option<&'static str>,
             about: &'static [&'static str],
             pre: &'static [&'static str],
             plots: &'static [(&'static str, &'static str)],
             window: Window,
             readings: Vec<Reading>| P {
        n,
        pin,
        about,
        pre,
        plots,
        tol: None,
        window,
        tier: 1,
        readings,
    };
    vec![
        p(
            120,
            Some("pin:06-int-width"),
            &["series int beyond 2^53 and 2^63"],
            F120_PRE,
            F120_PLOTS,
            Window::Head(64),
            f120_readings(),
        ),
        p(
            121,
            Some("pin:06-nonfinite"),
            &["where a non-finite value becomes na"],
            F121_PRE,
            F121_PLOTS,
            Window::Head(64),
            f121_readings(),
        ),
        p(
            122,
            Some("pin:U06-float-eq-v6"),
            &["float == and the ordering operators on series operands"],
            F122_PRE,
            F122_PLOTS,
            Window::Head(64),
            f122_readings(),
        ),
        p(
            123,
            Some("pin:U04-round-ties"),
            &["math.round_to_mintick near a decimal tie"],
            F123_PRE,
            F123_PLOTS,
            Window::Head(16),
            f123_readings(),
        ),
        p(
            124,
            Some("pin:02-fold-sig15-ties"),
            &["constant folding of + - * / and a 16th-digit tie"],
            &[],
            F124_PLOTS,
            Window::Head(4),
            f124_readings(),
        ),
        p(
            125,
            None,
            &["which additive chains TradingView re-associates", "CI-022"],
            F125_PRE,
            F125_PLOTS,
            Window::Head(16),
            Vec::new(),
        ),
        P {
            tier: 3,
            ..p(
                126,
                Some("pin:04-array-percentrank"),
                &["array.percentrank without na and with one"],
                F126_PRE,
                F126_PLOTS,
                Window::Head(1),
                f126_readings(),
            )
        },
        p(
            127,
            Some("pin:04-cog-tv-order"),
            &["math.sum: rolling or a fresh pass"],
            &["m@ = math.sum(syn, 14)"],
            &[("na(m@) ? 1 : 0", "isna"), ("na(m@) ? na : m@", "val")],
            Window::Head(32),
            f127_readings(),
        ),
        P {
            tol: Some((1, Tol::Ulp(12))),
            ..p(
                128,
                Some("pin:02-ta-in-loop-restart-scope"),
                &["math.sum inside a loop: per_call or restart"],
                &["acc@ = 0.0", "for i@ = 0 to 2", ">acc@ += math.sum(syn, 3)"],
                &[
                    ("na(acc@) ? 1 : 0", "isna"),
                    ("na(acc@) ? na : acc@", "val"),
                ],
                Window::Head(16),
                f128_readings(),
            )
        },
    ]
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
    let plots: Vec<(String, String)> = r
        .plots
        .iter()
        .map(|(e, k)| (e.replace('@', &sfx), k.to_string()))
        .collect();
    let mut tols = vec![E; plots.len()];
    if let Some((k, t)) = r.tol {
        tols[k] = t;
    }
    let pins: Vec<String> = r.pin.iter().map(|p| p.to_string()).collect();
    let script = Script {
        id,
        dialects: Dialects::BOTH,
        about: r.about.iter().map(|a| a.to_string()).collect(),
        twin: None,
        pins: pins.clone(),
        decl: None,
        prelude: Prelude::Syn1,
        body: r.pre.iter().map(|s| line(s, id)).collect(),
        plots,
    };
    Cell {
        script,
        title: r.about[0].to_string(),
        pins,
        oracle: Oracle::Tv,
        tier: r.tier,
        window: r.window,
        tols,
        expect: Expect::Pending,
        readings: r.readings,
        // r39 (capture 08): F125's `close` column reads the chart's close — TradingView ran every
        // sheet on the BTC chart from its first bar — so F125 is driven by the chart, aligned by
        // step like the rest of its sheet (bar 0 = the bar root's first row, as SYN-1's own rows);
        // a synthetic bar's close is not TradingView's (the r39 run's one F125 difference).
        run: if r.n == 125 {
            Run {
                driver: crate::fixture::Driver::Chart,
                ..Run::default()
            }
        } else {
            Run::default()
        },
    }
}

/// F120–F128, in id order.
pub fn cells() -> Vec<Cell> {
    rows().into_iter().map(cell_of).collect()
}

/// F123's fourteen inputs as the script forms them (for the twins' tests).
pub fn f123_x() -> [f64; 14] {
    f123_inputs()
}
