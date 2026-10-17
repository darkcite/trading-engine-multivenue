// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The Rust twins that author pin fixtures' `reading` blocks (impl-06 §4.9.4).
//!
//! GENERATOR-TIME ONLY (DOCTRINE law 2): every twin returns a fresh `Vec`; nothing here is
//! reachable from a step loop. A twin is a *reading* — one candidate semantics a pin fixture
//! discriminates — never a claim about TradingView. `na` is `f64::NAN` throughout and propagates
//! through arithmetic, as `F_*` does (contract `F_MIN`/`F_MAX` NaN-propagating, `pin:06-minmax-na`).
//!
//! Each twin follows the Pine reference form its doc names, operation for operation, so a reading
//! row is the bit pattern that form produces in binary64. `pin:06-twin-forms` — where the reference
//! manual gives no form, the literal one is written here and named at the function.

/// `na`.
pub const NA: f64 = f64::NAN;

/// `pin:06-r-p2-refuse-code` — the code F007 (a literal `/ 0.0`) and E056 (a negative `qty`
/// literal) are refused with. No `DiagCode` names either refusal yet (contract §3.3 lists none,
/// no CAR assigns one); impl-02 §3.4 row 7 (`TypeMismatch`, `3007`) is the class the spec gives
/// constant-value domain refusals ("a non-positive constant window length or `for` step, a
/// negative constant history offset, … a negative `pyramiding`"), so both read `3007` until pine-sema
/// assigns a dedicated code — then this constant (one line) follows it.
pub const R_P2_REFUSE: pine_ir::DiagCode = pine_ir::DiagCode(3007);

/// TradingView's v5 order cap (migration guide, *Excess orders are trimmed*: "when a strategy
/// creates more than 9000 orders, it raises a runtime error"; capture: RE10138 "Maximum number of
/// orders (9000) was reached.").
pub const V5_ORDER_LIMIT: i64 = 9000;

/// E110's fault row, computed from its script (`pin:06-e110-fault-row`): `t` flips every step and
/// each step places exactly one order (step `2j` `strategy.entry`, step `2j + 1` `strategy.close`
/// of the position the previous order opened), so order `n` is created on step `n − 1` and the
/// `V5_ORDER_LIMIT + 1`-th — the first past the cap — on step `V5_ORDER_LIMIT`; the
/// `V5_ORDER_LIMIT`-th order also fills at that step's open (`process_orders_on_close = false`).
/// Both readings of "where the cap bites" name the same row.
pub const fn e110_fault_row() -> i64 {
    V5_ORDER_LIMIT
}

/// R-P2 (operator ruling 2026-10-17): the five escalated fixtures adopt TradingView's captured
/// behaviour (vault `capture-texts.md`). Each gets one reading the lead's re-paste
/// (`pine-conf distil --paste`) records:
///
/// | id | reading | body | TradingView |
/// |---|---|---|---|
/// | F007 | `refuse_literal` | `refuse 3007` ([`R_P2_REFUSE`]) | compile error "Division by zero" |
/// | E056 | `refuse_literal` | `refuse 3007` | compile error "… cannot have a negative "qty" value …" |
/// | C029 | `runtime_fault` | `fault ScriptRefused 0` | "Error on bar 0: Cannot access the … field of an undefined object" |
/// | F085 | `runtime_fault` | `fault ScriptRefused 0` | "Error on bar 0: 'step' in loop must be greater than zero." |
/// | E110 | `runtime_fault` | `fault ScriptRefused 9000` ([`e110_fault_row`]) | RE10138 "Maximum number of orders (9000) was reached." |
///
/// `ScriptRefused` is contract §3.11's kind for "something Pine itself rejects at runtime" (its doc
/// names "a field of an `na` object"); impl-06 §4.2.5 already records E110 as `fault ScriptRefused
/// 9000`. Any other id: no reading.
pub fn escalated(id: &str) -> Vec<crate::fixture::Reading> {
    use crate::fixture::{Reading, ReadingBody};
    use pine_ir::FaultKind;
    let refuse = || ReadingBody::Refuse(R_P2_REFUSE);
    let fault = |row: i64| ReadingBody::Fault {
        kind: FaultKind::ScriptRefused,
        row,
    };
    let (name, body) = match id {
        "F007" | "E056" => ("refuse_literal", refuse()),
        "C029" | "F085" => ("runtime_fault", fault(0)),
        "E110" => ("runtime_fault", fault(e110_fault_row())),
        _ => return Vec::new(),
    };
    vec![Reading {
        name: name.to_string(),
        body,
    }]
}

fn hist(x: &[f64], i: usize, k: usize) -> f64 {
    if k <= i {
        x[i - k]
    } else {
        NA
    }
}

/// `ta.sma(x, len)`: the fresh sum of the last `len` values, oldest first, divided by `len`; `na`
/// until `len` values exist, and whenever one of them is `na`.
///
/// `pin:06-twin-sum-order` — a fresh oldest-first sum per bar (TradingView's accumulator shape is
/// unknown; §4.4.2 bounds the difference with `ulp`).
pub fn sma(x: &[f64], len: usize) -> Vec<f64> {
    (0..x.len())
        .map(|i| {
            if i + 1 < len {
                return NA;
            }
            let mut s = 0.0;
            for k in (0..len).rev() {
                s += x[i - k];
            }
            s / len as f64
        })
        .collect()
}

/// `ta.wma(x, len)`, the reference form: `weight = (len − i) · len`, `norm += weight`,
/// `sum += x[i] · weight` for `i = 0 .. len−1`, then `sum / norm`.
pub fn wma(x: &[f64], len: usize) -> Vec<f64> {
    (0..x.len())
        .map(|i| {
            if i + 1 < len {
                return NA;
            }
            let (mut norm, mut sum) = (0.0, 0.0);
            for k in 0..len {
                let w = ((len - k) * len) as f64;
                norm += w;
                sum += x[i - k] * w;
            }
            sum / norm
        })
        .collect()
}

/// `ta.stdev(x, len)` (biased): `sqrt(Σ (x[i] − sma)² / len)`, deviations summed newest first.
///
/// `pin:06-twin-stdev-epsilon` — the reference form's near-zero clamp is not applied.
pub fn stdev(x: &[f64], len: usize) -> Vec<f64> {
    let avg = sma(x, len);
    (0..x.len())
        .map(|i| {
            if i + 1 < len {
                return NA;
            }
            let mut ss = 0.0;
            for k in 0..len {
                let d = x[i - k] - avg[i];
                ss += d * d;
            }
            (ss / len as f64).sqrt()
        })
        .collect()
}

/// The EMA seed — the two readings of `ta.ema`'s first value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmaSeed {
    /// `sum := na(sum[1]) ? src : …` — the reference manual's form.
    First,
    /// The first value is `ta.sma(src, len)`, as `ta.rma`'s form seeds.
    Sma,
}

/// The recurrence `alpha · src + (1 − alpha) · prev`, seeded per `seed`.
fn recur(x: &[f64], alpha: f64, len: usize, seed: EmaSeed) -> Vec<f64> {
    let seed_sma = sma(x, len);
    let mut out = Vec::with_capacity(x.len());
    let mut prev = NA;
    for i in 0..x.len() {
        let v = if prev.is_nan() {
            match seed {
                EmaSeed::First => x[i],
                EmaSeed::Sma => seed_sma[i],
            }
        } else {
            alpha * x[i] + (1.0 - alpha) * prev
        };
        out.push(v);
        prev = v;
    }
    out
}

/// `ta.ema(x, len)`: `alpha = 2 / (len + 1)`.
pub fn ema(x: &[f64], len: usize, seed: EmaSeed) -> Vec<f64> {
    recur(x, 2.0 / (len as f64 + 1.0), len, seed)
}

/// `ta.rma(x, len)`: `alpha = 1 / len`, seeded with `ta.sma(x, len)`.
pub fn rma(x: &[f64], len: usize) -> Vec<f64> {
    recur(x, 1.0 / len as f64, len, EmaSeed::Sma)
}

/// `ta.highest(x, len)`: the maximum of the last `len` values; `na` until `len` exist or when one
/// is `na` (`pin:06-twin-highest-na`).
pub fn highest(x: &[f64], len: usize) -> Vec<f64> {
    (0..x.len())
        .map(|i| {
            if i + 1 < len {
                return NA;
            }
            let mut m = x[i + 1 - len];
            for k in (0..len - 1).rev() {
                let v = x[i - k];
                if v.is_nan() || m.is_nan() {
                    m = NA;
                } else if v > m {
                    m = v;
                }
            }
            m
        })
        .collect()
}

/// `ta.change(x, len)` = `x − x[len]`.
pub fn change(x: &[f64], len: usize) -> Vec<f64> {
    (0..x.len()).map(|i| x[i] - hist(x, i, len)).collect()
}

/// `ta.rising(x, len)`: `x > x[k]` for every `k = 1..=len`, as `1`/`0`; `na` while `x[len]` does
/// not exist (`pin:06-twin-rising-warmup`).
pub fn rising(x: &[f64], len: usize) -> Vec<f64> {
    mono(x, len, |a, b| a > b)
}

/// `ta.falling(x, len)`: `x < x[k]` for every `k = 1..=len`; warm-up as [`rising`].
pub fn falling(x: &[f64], len: usize) -> Vec<f64> {
    mono(x, len, |a, b| a < b)
}

fn mono(x: &[f64], len: usize, cmp: fn(f64, f64) -> bool) -> Vec<f64> {
    (0..x.len())
        .map(|i| {
            if i < len {
                return NA;
            }
            let all = (1..=len).all(|k| cmp(x[i], x[i - k]));
            if all {
                1.0
            } else {
                0.0
            }
        })
        .collect()
}

/// `ta.barssince(cond)`: bars since `cond` was last true (0 on a true bar); `na` before the first.
pub fn barssince(cond: &[bool]) -> Vec<f64> {
    let mut last: Option<usize> = None;
    (0..cond.len())
        .map(|i| {
            if cond[i] {
                last = Some(i);
            }
            last.map_or(NA, |j| (i - j) as f64)
        })
        .collect()
}

/// `ta.macd(x, fast, slow, signal)` → `(macd, signal, hist)` over [`ema`] with `seed`.
pub fn macd(
    x: &[f64],
    fast: usize,
    slow: usize,
    sig: usize,
    seed: EmaSeed,
) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let f = ema(x, fast, seed);
    let s = ema(x, slow, seed);
    let m: Vec<f64> = (0..x.len()).map(|i| f[i] - s[i]).collect();
    let g = ema(&m, sig, seed);
    let h = (0..x.len()).map(|i| m[i] - g[i]).collect();
    (m, g, h)
}

/// NaN-propagating `math.max` (contract `F_MAX`).
fn fmax(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        NA
    } else if a > b {
        a
    } else {
        b
    }
}

/// `ta.rsi(x, len)`, the reference form: `u = max(x − x[1], 0)`, `d = max(x[1] − x, 0)`,
/// `rs = rma(u, len) / rma(d, len)`, `100 − 100 / (1 + rs)`.
pub fn rsi(x: &[f64], len: usize) -> Vec<f64> {
    let u: Vec<f64> = (0..x.len())
        .map(|i| fmax(x[i] - hist(x, i, 1), 0.0))
        .collect();
    let d: Vec<f64> = (0..x.len())
        .map(|i| fmax(hist(x, i, 1) - x[i], 0.0))
        .collect();
    let (ru, rd) = (rma(&u, len), rma(&d, len));
    (0..x.len())
        .map(|i| 100.0 - 100.0 / (1.0 + ru[i] / rd[i]))
        .collect()
}

/// The `math.round` tie readings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundTie {
    /// `2.5 → 3`, `−2.5 → −3`.
    Away,
    /// `2.5 → 2`, `3.5 → 4`.
    Even,
    /// Toward +∞: `2.5 → 3`, `−2.5 → −2`.
    Up,
}

/// `math.round(x)` under `tie`.
pub fn round(x: f64, tie: RoundTie) -> f64 {
    match tie {
        RoundTie::Away => x.round(),
        RoundTie::Even => x.round_ties_even(),
        RoundTie::Up => {
            // `x − floor(x)` is exact; `(x + 0.5).floor()` is not (0.49999999999999994 → 1).
            let f = x.floor();
            if x - f >= 0.5 {
                f + 1.0
            } else {
                f
            }
        }
    }
}

/// Floored modulo: the result has the divisor's sign (`-1 % 100 = 99`); `b == 0` → `None` (`na`).
pub fn mod_floored(a: i64, b: i64) -> Option<i64> {
    (b != 0).then(|| a.wrapping_rem(b)).map(|r| {
        if r != 0 && ((r < 0) != (b < 0)) {
            r.wrapping_add(b)
        } else {
            r
        }
    })
}

/// Truncated modulo: the result has the dividend's sign (`-1 % 100 = -1`); `b == 0` → `None`.
pub fn mod_truncated(a: i64, b: i64) -> Option<i64> {
    (b != 0).then(|| a.wrapping_rem(b))
}

/// `pin:06-cond-call-state`, reading `executed_only`: a stateful call inside an `if` advances only
/// on the bars where `cond` holds — `f` sees the executed subsequence; other bars yield `na`.
pub fn executed_only(x: &[f64], cond: &[bool], f: impl Fn(&[f64]) -> Vec<f64>) -> Vec<f64> {
    let idx: Vec<usize> = (0..x.len()).filter(|&i| cond[i]).collect();
    let sub: Vec<f64> = idx.iter().map(|&i| x[i]).collect();
    let fy = f(&sub);
    let mut out = vec![NA; x.len()];
    for (j, &i) in idx.iter().enumerate() {
        out[i] = fy[j];
    }
    out
}

/// `pin:06-cond-call-state`, reading `every_bar`: the call advances on every bar; it is observed
/// only where `cond` holds, `na` elsewhere.
pub fn every_bar(x: &[f64], cond: &[bool], f: impl Fn(&[f64]) -> Vec<f64>) -> Vec<f64> {
    let fy = f(x);
    (0..x.len())
        .map(|i| if cond[i] { fy[i] } else { NA })
        .collect()
}

/// An authored cell (§4.4.1): `na` for NaN, else the shortest round-trip rendering (`{:?}`).
///
/// `pin:06-authored-cell` — NaN is the only value written `na`; `±inf` print as `inf` / `-inf`.
pub fn cell(v: f64) -> crate::fixture::Cell {
    if v.is_nan() {
        crate::fixture::Cell::Na
    } else {
        crate::fixture::Cell::Val {
            v,
            text: format!("{v:?}"),
        }
    }
}

/// One row of authored cells.
pub fn row(key: i64, vals: &[f64]) -> crate::fixture::Row {
    crate::fixture::Row {
        key,
        cells: vals.iter().map(|&v| cell(v)).collect(),
    }
}

/// The isna/val pair's two cells for a value (§4.2.0): `1, na` for `na`, else `0, v`.
pub fn pair(v: f64) -> [f64; 2] {
    if v.is_nan() {
        [1.0, NA]
    } else {
        [0.0, v]
    }
}

/// `PROJ` of a color (§4.2.0): `r · 65536 + g · 256 + b` of a `#RRGGBB` value.
pub const fn proj_rgb(rgb: u32) -> f64 {
    let r = (rgb >> 16) & 0xff;
    let g = (rgb >> 8) & 0xff;
    let b = rgb & 0xff;
    (r * 65536 + g * 256 + b) as f64
}

/// `pin:03-float-mod`, reading `as_written`: `a − b · ⌊a / b⌋` in binary64, operation for operation
/// (impl-03's `F_MOD` arm).
pub fn fmod_as_written(a: f64, b: f64) -> f64 {
    a - b * (a / b).floor()
}

/// `pin:03-float-mod`, reading `exact_remainder`: the exact floored remainder of the two binary64
/// operands, rounded once. The sign is `pin:06-mod-sign`'s floored rule.
///
/// `pin:06-f098-exact-form` — IEEE `fmod` (Rust `%` on `f64`, exact, the dividend's sign), then
/// `+ b` when the remainder is non-zero and its sign differs from `b`'s.
pub fn fmod_exact(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r != 0.0 && ((r < 0.0) != (b < 0.0)) {
        r + b
    } else {
        r
    }
}

/// `pin:U03-rising-falling`, reading `monotone`: `x[0] > x[1] > … > x[len]` (strictly, each step);
/// warm-up as [`rising`]. `up = false` is the falling form.
pub fn monotone(x: &[f64], len: usize, up: bool) -> Vec<f64> {
    (0..x.len())
        .map(|i| {
            if i < len {
                return NA;
            }
            let all = (0..len).all(|k| {
                let (a, b) = (x[i - k], x[i - k - 1]);
                if up {
                    a > b
                } else {
                    a < b
                }
            });
            if all {
                1.0
            } else {
                0.0
            }
        })
        .collect()
}

/// `pin:W5-sma-na`, reading `mean_of_valid`: the mean of the non-`na` values among the last `len`,
/// summed oldest first; `na` until `len` values exist or when none is valid.
pub fn sma_mean_of_valid(x: &[f64], len: usize) -> Vec<f64> {
    (0..x.len())
        .map(|i| {
            if i + 1 < len {
                return NA;
            }
            let (mut s, mut c) = (0.0, 0usize);
            for k in (0..len).rev() {
                let v = x[i - k];
                if !v.is_nan() {
                    s += v;
                    c += 1;
                }
            }
            if c == 0 {
                NA
            } else {
                s / c as f64
            }
        })
        .collect()
}

/// `pin:W5-sma-na`, reading `last_n_valid`: the mean of the last `len` non-`na` values (the window
/// reaches back past `na`s), summed oldest first; `na` until `len` valid values exist.
pub fn sma_last_n_valid(x: &[f64], len: usize) -> Vec<f64> {
    (0..x.len())
        .map(|i| {
            let valid: Vec<f64> = x[..=i].iter().copied().filter(|v| !v.is_nan()).collect();
            if valid.len() < len {
                return NA;
            }
            let mut s = 0.0;
            for v in &valid[valid.len() - len..] {
                s += v;
            }
            s / len as f64
        })
        .collect()
}

// ---- r29 (2026-10-17): the readings of the next capture session, written before it (§9) --------

/// The `C` of a linearly interpolated percentile (`pin:04-percentile-linear-c-half`, R-P15): the
/// "linear interpolation between closest ranks" family. With `N` values and `p` in percent, the
/// 0-based rank is `x = (p · N) / 100 − 1/2` (`Half`, pine-std's `ta.*` default since R-P15),
/// `(p · (N + 1)) / 100 − 1` (`Zero`) or `(p · (N − 1)) / 100` (`One`, Excel `PERCENTILE.INC`,
/// pine-std's `array.*` row today), clamped to `[0, N − 1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PctC {
    /// `C = 1/2`.
    Half,
    /// `C = 0`.
    Zero,
    /// `C = 1`.
    One,
}

/// `percentile_linear_interpolation` of `vals` (no `na`) at `p` under `c`: the ascending order
/// statistics `w`, `k = ⌊x⌋`, `w₍ₖ₎ + (x − k) · (w₍ₖ₊₁₎ − w₍ₖ₎)` — pine-std's operation order
/// (`coll/order.rs` `linear`, `ta/rank.rs` `linear`). `pin:06-twin-pct-form`.
pub fn pct_linear(vals: &[f64], p: f64, c: PctC) -> f64 {
    // COPY: `vals` (one fixture's ≤ 14 values) into a sorted scratch — generation time, cold —
    // sorting the caller's slice in place rejected: the caller keeps the series order.
    let mut w = vals.to_vec();
    w.sort_by(f64::total_cmp);
    let n = w.len();
    let x = match c {
        PctC::Half => (p * n as f64) / 100.0 - 0.5,
        PctC::Zero => (p * (n + 1) as f64) / 100.0 - 1.0,
        PctC::One => (p * (n - 1) as f64) / 100.0,
    };
    let x = x.clamp(0.0, (n - 1) as f64);
    let k = x.floor() as usize;
    let frac = x - k as f64;
    if frac == 0.0 || k + 1 >= n {
        w[k]
    } else {
        w[k] + frac * (w[k + 1] - w[k])
    }
}

/// `ta.percentile_linear_interpolation(x, len, p)` under `c`: `na` until `len` values exist.
pub fn ta_pct_linear(x: &[f64], len: usize, p: f64, c: PctC) -> Vec<f64> {
    (0..x.len())
        .map(|i| {
            if i + 1 < len {
                NA
            } else {
                pct_linear(&x[i + 1 - len..=i], p, c)
            }
        })
        .collect()
}

/// `pin:04-binary-search`'s two readings of `binary_search_leftmost` for a value below every
/// element: `MinusOne` (pine-std's pin: "the next smaller element to the left" does not exist)
/// and `Zero` (the v6 reference's statement, lead r29).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftBelow {
    /// `−1`.
    MinusOne,
    /// `0`.
    Zero,
}

/// `array.binary_search` (`mode` 0) / `_leftmost` (1) / `_rightmost` (2) of `v` in the ascending
/// `w` (no duplicates in the r29 fixture, so "which occurrence" is not asked): plain → the index
/// or −1; leftmost → the index, else that of the next smaller element (below every element:
/// `below`); rightmost → the index, else that of the element to the right of where `v` would lie
/// (above every element: the size).
pub fn bsearch(w: &[f64], v: f64, mode: u8, below: LeftBelow) -> f64 {
    let lo = w.iter().take_while(|&&x| x < v).count();
    let found = lo < w.len() && w[lo] == v;
    let r = match (mode, found) {
        (_, true) => lo as i64,
        (0, false) => -1,
        (1, false) if lo == 0 => match below {
            LeftBelow::MinusOne => -1,
            LeftBelow::Zero => 0,
        },
        (1, false) => lo as i64 - 1,
        _ => lo as i64,
    };
    r as f64
}

/// `pin:04-udt-sort-ties`, reading `heap_order`: the input positions in the order pine-std's
/// in-place heap sort (`coll/part3.rs` `heap_sort` / `sift` under `before_obj`: strict `<`, or
/// strict `>` descending, ties unbroken) leaves them — operation for operation.
pub fn heap_sort_order(keys: &[f64], desc: bool) -> Vec<usize> {
    let before = |a: usize, b: usize| {
        if desc {
            keys[a] > keys[b]
        } else {
            keys[a] < keys[b]
        }
    };
    let sift = |p: &mut [usize], root: usize, end: usize| {
        let mut r = root;
        loop {
            let mut ch = 2 * r + 1;
            if ch >= end {
                break;
            }
            if ch + 1 < end && before(p[ch], p[ch + 1]) {
                ch += 1;
            }
            if !before(p[r], p[ch]) {
                break;
            }
            p.swap(r, ch);
            r = ch;
        }
    };
    let n = keys.len();
    let mut p: Vec<usize> = (0..n).collect();
    let mut s = n / 2;
    while s > 0 {
        s -= 1;
        sift(&mut p, s, n);
    }
    let mut end = n;
    while end > 1 {
        end -= 1;
        p.swap(0, end);
        sift(&mut p, 0, end);
    }
    p
}

/// `pin:04-udt-sort-ties`, reading `stable`: equal keys keep their input order.
pub fn stable_sort_order(keys: &[f64], desc: bool) -> Vec<usize> {
    let mut p: Vec<usize> = (0..keys.len()).collect();
    p.sort_by(|&a, &b| {
        let o = keys[a].total_cmp(&keys[b]);
        if desc {
            o.reverse()
        } else {
            o
        }
    });
    p
}

/// History ticks (R-P6, impl-05 §4.8.9, CAR-D1): the per-bar constants each reading predicts for
/// E122 (`runs`, `conf`: executions per history bar, and those with `barstate.isconfirmed`) —
/// `pin:05-hist-tick-runs`. `three_points_and_close` is impl-05's provisional reading (the open
/// and the two extremes in path order, then the confirmed bar close); the *strategy()* reference
/// ("a new execution … on every available tick in each historical bar. Historical ticks refer to
/// bar OHLC prices") also reads as four tick executions at O, H/L, L/H, C before the bar-close
/// execution (`four_ticks_and_close`), or as the extremes and the close only, the open being the
/// previous close's continuation (`extremes_and_close`).
pub const HIST_TICK_RUNS: [(&str, [f64; 2]); 3] = [
    ("three_points_and_close", [4.0, 1.0]),
    ("four_ticks_and_close", [5.0, 1.0]),
    ("extremes_and_close", [3.0, 1.0]),
];

/// E123's per-bar constants (`closenew`: `barstate.isnew` at the bar-close execution; `tnew`:
/// tick executions with `barstate.isnew`) — `pin:U17-hist-close-isnew`. Both readings take
/// CAR-D1's tick column (`isnew` on a bar's first history tick only, `tnew = 1`); they differ in
/// the bar column: `close_isnew` (contract §3.6.5 unchanged, impl-03's provisional) /
/// `close_not_new` (as a realtime bar's closing update, *Bar states*: `isnew` only on the bar's
/// first execution).
pub const HIST_TICK_ISNEW: [(&str, [f64; 2]); 2] =
    [("close_isnew", [1.0, 1.0]), ("close_not_new", [0.0, 1.0])];

/// The r29 broker probes whose readings predict List-of-Trades rows (chart prices): named here
/// before capture, decided by reading the captured trades against the prediction (§4.9.4,
/// `pin:06-r29-broker-readings`) — `(fixture, pin, reading, prediction)`; the provisional default
/// first. E129 / E131 place their level at `+12.4` / `+12.6` ticks from the order bar's close (by
/// `syni`'s parity), E130 at `−20.4` / `−20.6` ticks from the average price; E132 is CI-042.
pub const BROKER_READINGS: &[(&str, &str, &str, &str)] = &[
    (
        "E124",
        HT_NEXT,
        "next_point",
        "an entry born at the open's run fills at point 1, same bar",
    ),
    (
        "E124",
        HT_NEXT,
        "bar_close",
        "it fills at the same bar's close",
    ),
    (
        "E124",
        HT_NEXT,
        "next_bar_open",
        "it fills at the next bar's open",
    ),
    (
        "E125",
        HT_IMM,
        "same_point",
        "close(immediately) born at point 1's run fills at point 1",
    ),
    (
        "E125",
        HT_IMM,
        "next_point",
        "it fills at point 2 (the second extreme)",
    ),
    (
        "E126",
        HT_COF,
        "cof_first",
        "fill bars: runs 5, first 1 (the fill's run precedes the tick's)",
    ),
    (
        "E126",
        HT_COF,
        "tick_first",
        "fill bars: runs 5, first 0 (the open's tick run first)",
    ),
    (
        "E126",
        HT_COF,
        "merged",
        "fill bars: runs 4 (one execution serves both)",
    ),
    (
        "E127",
        HT_FILL,
        "fill_then_run",
        "p1 = 1 on the bar whose open fills the entry",
    ),
    (
        "E127",
        HT_FILL,
        "run_then_fill",
        "p1 = 0 on that bar (the fill shows from the next run)",
    ),
    (
        "E129 E130 E131",
        ROUND,
        "floor_tick",
        "+12.4 / +12.6 ticks fill at +12 / +12; -20.4 / -20.6 at -21 / -21",
    ),
    (
        "E129 E130 E131",
        ROUND,
        "nearest_tick",
        "+12 / +13; -20 / -21",
    ),
    ("E129 E130 E131", ROUND, "ceil_tick", "+13 / +13; -20 / -20"),
    (
        "E132",
        FUNDS,
        "reject",
        "an entry whose fill open exceeds its order bar's close: no trade",
    ),
    (
        "E132",
        FUNDS,
        "reduce",
        "it fills with the quantity the funds allow at the fill price",
    ),
    (
        "E132",
        FUNDS,
        "fill_whole",
        "it fills at the order-bar-close quantity (a margin call may follow)",
    ),
];

const HT_NEXT: &str = "pin:05-hist-tick-next-point";
const HT_IMM: &str = "pin:05-hist-tick-immediately";
const HT_COF: &str = "pin:05-hist-tick-cof";
const HT_FILL: &str = "pin:05-hist-tick-fill-first";
const ROUND: &str = "pin:05-price-rounding";
const FUNDS: &str = "pin:05-margin-funds-check";
