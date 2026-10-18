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
///
/// R-P18 (operator 2026-10-17): TradingView ran 4 executions per history bar with
/// `barstate.isconfirmed` true on all 4 (E122 `runs` 4, `conf` 4) —
/// `three_points_and_close_confirmed`, the three tick points and the close with every
/// history-tick run reading `isconfirmed` true (CAR-R32-1, applied in pine-vm by the lead).
pub const HIST_TICK_RUNS: [(&str, [f64; 2]); 4] = [
    ("three_points_and_close", [4.0, 1.0]),
    ("four_ticks_and_close", [5.0, 1.0]),
    ("extremes_and_close", [3.0, 1.0]),
    ("three_points_and_close_confirmed", [4.0, 4.0]),
];

/// E123's per-bar constants (`closenew`: `barstate.isnew` at the bar-close execution; `tnew`:
/// tick executions with `barstate.isnew and not barstate.isconfirmed`) —
/// `pin:U17-hist-close-isnew`. The readings differ in the bar column: `close_isnew` (contract
/// §3.6.5 unchanged, impl-03's provisional) / `close_not_new` (as a realtime bar's closing
/// update, *Bar states*: `isnew` only on the bar's first execution). R-P18 (2026-10-17): the
/// tick column follows CAR-R32-1 — a history-tick run reads `isconfirmed` true (E122 `conf` 4),
/// so no tick run counts and `tnew = 0` in both (it was CAR-D1's `1`, with ticks unconfirmed).
/// TradingView's E123: `closenew` 1, `tnew` 0 — `close_isnew`.
pub const HIST_TICK_ISNEW: [(&str, [f64; 2]); 2] =
    [("close_isnew", [1.0, 0.0]), ("close_not_new", [0.0, 0.0])];

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
    // R-P18 (2026-10-17): TradingView's outcome (E124 45/45, `pin:05-hist-tick-same-point`).
    (
        "E124",
        HT_NEXT,
        "same_point",
        "it fills at point 0 (the open), right after the open's run",
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
    // r34 (2026-10-18, R-P26): the probe sheet r34's broker probes (P9–P11), predictions as the
    // sheet states them; the first trade's order bar close 77249.5 is E069's (P10).
    (
        "E133",
        FEES,
        "fill_price",
        "fee = 0.001 * 10 * px, px the slipped fill (px 78865.1 -> 788.651)",
    ),
    (
        "E133",
        FEES,
        "pre_slippage",
        "fee = 0.001 * 10 * (px - 5.0), the price before 50 ticks of slippage (-> 788.601)",
    ),
    (
        "E133",
        FEES,
        "cent_rounded",
        "the fill_price fee cut to cents (-> 788.65)",
    ),
    (
        "E134 E135 E136 E137",
        W18,
        "v5_default_0",
        "W18 as documented, no cap: 32.362669 / 32.362669 / the funds check / 32.362669",
    ),
    (
        "E134 E135 E136 E137",
        W18,
        "v5_default_100",
        "the v5 default is 100, the funds check reduces: 12.945067 / 32.362669 / 12.945067 / 32.362669",
    ),
    (
        "E134 E135 E136 E137",
        W18,
        "equity_cap",
        "a 100 % of equity sizing cap whatever the margin: 12.945067 on all four",
    ),
    (
        "E136",
        FUNDS,
        "reduce",
        "under v5_default_0: 12.945067 (the quantity the funds allow)",
    ),
    (
        "E136",
        FUNDS,
        "reject",
        "under v5_default_0: no trade",
    ),
    (
        "E138",
        EXIT_QTY,
        "per_trade",
        concat!(
            "ours (the engine, r37): after the first take-profit bar pos / ot / ct / s0 / s1 = ",
            "2 / 1 / 2 / 2 / na — a leg of min(1, 2) per covered trade, FIFO closing both from ",
            "the oldest trade (the sheet's 2 / 2 / 2 / 1 / 1 has each leg close its own trade)"
        ),
    ),
    (
        "E138",
        EXIT_QTY,
        "per_call",
        "3 / 2 / 1 / 1 / 2 (one budget of 1, oldest trade first)",
    ),
    (
        "E138",
        EXIT_QTY,
        "per_position",
        "3 / 2 / 1 / 1 / 2 (one leg of 1 for the position; told from per_call by the exit rows)",
    ),
    (
        "E138",
        EXIT_QTY,
        "second_entry_refused",
        "ot 1 before any exit, b1 na (the pyramided second entry is not taken)",
    ),
];

const HT_NEXT: &str = "pin:05-hist-tick-next-point";
const HT_IMM: &str = "pin:05-hist-tick-immediately";
const HT_COF: &str = "pin:05-hist-tick-cof";
const HT_FILL: &str = "pin:05-hist-tick-fill-first";
const ROUND: &str = "pin:05-price-rounding";
const FUNDS: &str = "pin:05-margin-funds-check";
const FEES: &str = "pin:05-net-f64-fees-provisional";
const W18: &str = "pin:W18-margin-default";
const EXIT_QTY: &str = "pin:05-exit-qty-per-trade";

// ---- r34 (2026-10-18, R-P26): the probe sheet r34's readings, written before its capture --------

/// Neumaier's compensated sum of `xs` in order — pine-std's `state::neumaier` fold (`ta.sma`'s
/// `win_fresh_sum`, `ewma_step`'s SMA seed, the `pin:04-sma-fresh-sum` law).
pub fn neumaier_sum(xs: &[f64]) -> f64 {
    let (mut s, mut c) = (0.0f64, 0.0f64);
    for &v in xs {
        let t = s + v;
        c += if s.abs() >= v.abs() {
            (s - t) + v
        } else {
            (v - t) + s
        };
        s = t;
    }
    s + c
}

/// The plain sum of `xs` in order.
pub fn plain_sum(xs: &[f64]) -> f64 {
    let mut s = 0.0;
    for &v in xs {
        s += v;
    }
    s
}

/// pine-std's rolling window (`state::win_push` / `win_repoint`, `math.sum`'s block), operation
/// for operation: the evicted value is subtracted before the new one is added, and every
/// `WIN_RECOMPUTE` accepted values the sum is recomputed plainly, newest first. A twin, `Clone`
/// so a restart can copy the committed block.
#[derive(Debug, Clone, PartialEq)]
pub struct RollSum {
    win: Vec<f64>,
    len: usize,
    sum: f64,
    recompute: u32,
}

/// pine-std `state::WIN_RECOMPUTE`.
pub const WIN_RECOMPUTE: u32 = 1024;

impl RollSum {
    /// A cold window of `len` (its first execution's `win_repoint`).
    pub fn new(len: usize) -> Self {
        Self {
            win: Vec::with_capacity(len + 1),
            len,
            sum: 0.0,
            recompute: WIN_RECOMPUTE,
        }
    }

    /// `math.sum(v, len)` on one execution (a non-na `v`): the sum once `len` values exist.
    pub fn push(&mut self, v: f64) -> f64 {
        if self.win.len() == self.len {
            self.sum -= self.win.remove(0);
        }
        self.sum += v;
        self.win.push(v);
        if self.recompute == 0 {
            let mut s = 0.0;
            for k in (0..self.win.len()).rev() {
                s += self.win[k];
            }
            self.sum = s;
            self.recompute = WIN_RECOMPUTE;
        } else {
            self.recompute -= 1;
        }
        if self.win.len() < self.len {
            NA
        } else {
            self.sum
        }
    }
}

/// pine-std's `ta.sma` block (`pin:04-sma-fresh-sum`): the last `len` values, a fresh Neumaier sum
/// oldest first, `/ len`; `na` until `len` values exist.
#[derive(Debug, Clone, PartialEq)]
pub struct FreshSma {
    win: Vec<f64>,
    len: usize,
}

impl FreshSma {
    /// A cold window.
    pub fn new(len: usize) -> Self {
        Self {
            win: Vec::with_capacity(len + 1),
            len,
        }
    }

    /// One execution on a non-na `v`.
    pub fn push(&mut self, v: f64) -> f64 {
        if self.win.len() == self.len {
            self.win.remove(0);
        }
        self.win.push(v);
        if self.win.len() < self.len {
            NA
        } else {
            neumaier_sum(&self.win) / self.len as f64
        }
    }
}

/// pine-std's `ta.ema` block (`state::ewma_step`, `Smoother::Ema`, `pin:04-ewma-tv-arith`) under
/// U01 `sma` (decided, CI-014): the first `len` values summed Neumaier-compensated, `/ len`, then
/// `v + α·(x − v)`, `α = 2 / (len + 1)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmaSma {
    value: f64,
    acc: f64,
    n: u32,
    seeded: bool,
}

impl EmaSma {
    /// A cold block.
    pub const fn new() -> Self {
        Self {
            value: 0.0,
            acc: 0.0,
            n: 0,
            seeded: false,
        }
    }

    /// One execution on a non-na `v`.
    pub fn push(&mut self, v: f64, len: u32) -> f64 {
        self.n = self.n.saturating_add(1);
        if self.seeded {
            let a = 2.0 / (f64::from(len) + 1.0);
            self.value += a * (v - self.value);
            return self.value;
        }
        let t = self.acc + v;
        self.value += if self.acc.abs() >= v.abs() {
            (self.acc - t) + v
        } else {
            (v - t) + self.acc
        };
        self.acc = t;
        if self.n >= len {
            self.value = (self.acc + self.value) / f64::from(len);
            self.seeded = true;
            self.value
        } else {
            NA
        }
    }
}

impl Default for EmaSma {
    fn default() -> Self {
        Self::new()
    }
}

/// U13 `restart` (CAR-R33-9, decided by B068 B069): on every bar each of the `execs` executions
/// of an in-loop site starts from the block committed at the previous bar, and the block the last
/// execution leaves is kept. `exec(block, bar, k)` is one execution; per bar, the outputs in order.
pub fn restart_runs<S: Clone>(
    init: S,
    bars: usize,
    execs: usize,
    mut exec: impl FnMut(&mut S, usize, usize) -> f64,
) -> Vec<Vec<f64>> {
    let mut committed = init;
    let mut out = Vec::with_capacity(bars);
    for t in 0..bars {
        let mut row = Vec::with_capacity(execs);
        let mut last = None;
        for k in 0..execs {
            // COPY: the committed block per execution (≤ 3 per bar, generator time) — the law
            // itself (every execution starts from the committed image); a shared block rejected:
            // it is `per_call`.
            let mut b = committed.clone();
            row.push(exec(&mut b, t, k));
            last = Some(b);
        }
        if let Some(b) = last {
            committed = b;
        }
        out.push(row);
    }
    out
}

/// U13 `per_call`: one block advanced by every execution, in order; per bar, the outputs.
pub fn per_call_runs<S>(
    init: S,
    bars: usize,
    execs: usize,
    mut exec: impl FnMut(&mut S, usize, usize) -> f64,
) -> Vec<Vec<f64>> {
    let mut block = init;
    (0..bars)
        .map(|t| (0..execs).map(|k| exec(&mut block, t, k)).collect())
        .collect()
}

/// `acc = 0.0; acc += out` over one bar's executions, in order (B068 B069 F128's accumulator).
pub fn acc_sum(outs: &[f64]) -> f64 {
    let mut acc = 0.0;
    for &o in outs {
        acc += o;
    }
    acc
}

/// sema's constant fold of a float to 15 significant digits (`fold::sig15`, CAR-R33-4): Rust's
/// `{:.14e}`, which rounds an exact decimal tie to even (`pin:02-fold-sig15-ties`); non-finite
/// passes through.
pub fn sig15_even(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.14e}").parse().unwrap_or(NA)
}

/// 15 significant digits with an exact decimal tie away from zero (the probe sheet's P5
/// alternative): the exact decimal expansion (`{:.767e}`, every digit of a binary64), cut after
/// 15 digits, rounded up in magnitude when the rest is ≥ one half.
pub fn sig15_away(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let s = format!("{:.767e}", x.abs());
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let mut keep: Vec<u8> = mant.bytes().filter(u8::is_ascii_digit).collect();
    let up = keep.get(15).is_some_and(|&d| d >= b'5');
    keep.truncate(15);
    if up {
        let mut i = keep.len();
        loop {
            if i == 0 {
                keep.insert(0, b'1');
                break;
            }
            i -= 1;
            if keep[i] == b'9' {
                keep[i] = b'0';
            } else {
                keep[i] += 1;
                break;
            }
        }
    }
    let mut e: i32 = exp.parse().unwrap_or(0);
    if keep.len() > 15 {
        keep.truncate(15);
        e += 1;
    }
    let mut text = String::with_capacity(24);
    for (i, &d) in keep.iter().enumerate() {
        text.push(char::from(d));
        if i == 0 {
            text.push('.');
        }
    }
    text.push_str(&format!("e{e}"));
    let v: f64 = text.parse().unwrap_or(NA);
    v.copysign(x)
}

/// CAR-R33-7's `abs_1e10`: `|a − b| ≤ 1e-10`, false on `na`.
pub fn eq_tol(a: f64, b: f64) -> bool {
    !(a.is_nan() || b.is_nan()) && (a - b).abs() <= 1e-10
}

/// The probe sheet's P3 reading "tolerance on every comparison": `a > b` iff `a − b > 1e-10`
/// (false on `na`); `a ≥ b` is `a > b` or `eq_tol`.
pub fn gt_tol(a: f64, b: f64) -> bool {
    a - b > 1e-10
}

/// See [`gt_tol`].
pub fn ge_tol(a: f64, b: f64) -> bool {
    gt_tol(a, b) || eq_tol(a, b)
}

/// `pin:U04-round-ties`' R-int — pine-std's `math.round_to_mintick` (`math::bi_round_to_mintick`)
/// operation for operation: `to_1e6(x)` (half away), an integer tie (`half_up`, else away from
/// zero), the multiple of `mintick_1e6` back through `from_1e6`.
pub fn mintick_int(x: f64, mintick_1e6: i64, half_up: bool) -> f64 {
    if !x.is_finite() {
        return NA;
    }
    let v = (x * 1e6).round() as i64;
    let m = mintick_1e6;
    let (q, rem) = (v.div_euclid(m), v.rem_euclid(m));
    let up = 2 * i128::from(rem) > i128::from(m)
        || (2 * i128::from(rem) == i128::from(m) && (half_up || v > 0));
    let q = if up { q + 1 } else { q };
    q.checked_mul(m).map_or(NA, |t| t as f64 / 1e6)
}

/// The probe sheet's R-f64: `round(x / mintick) · mintick` with a half-up `round` of the f64
/// quotient, the product formed in binary64 as written.
pub fn mintick_f64(x: f64, mintick: f64) -> f64 {
    round(x / mintick, RoundTie::Up) * mintick
}

/// The probe sheet's R-band: `x` rounds up iff the **exact** distance of `|x|` below the decimal
/// tie is at most `1 / band_den` ticks (`1e-10` ticks: `band_den = 10¹⁰`, CAR-R33-7's constant),
/// else to the nearest tick; an exact tie away from zero (U04 `half_away`, decided); the sign
/// restored. Exact rational arithmetic on the binary64 (`|x| = m · 2^e`).
///
/// `pin:06-r34-band-formation` — the result is the correctly rounded decimal `ticks / D`
/// (`D = 10⁶ / mintick_1e6`), the formation the captured events show (E058 trade 275: 78058.4,
/// where `ticks × 0.1` forms 78058.40000000001).
pub fn mintick_band(x: f64, mintick_1e6: i64, band_den: u128) -> f64 {
    if !x.is_finite() {
        return NA;
    }
    debug_assert!(mintick_1e6 > 0 && 1_000_000 % mintick_1e6 == 0);
    let d = (1_000_000 / mintick_1e6) as u128;
    let bits = x.abs().to_bits();
    let (exp, frac) = ((bits >> 52) & 0x7ff, bits & ((1u64 << 52) - 1));
    let (m, e) = if exp == 0 {
        (u128::from(frac), -1074i64)
    } else {
        (u128::from(frac | (1u64 << 52)), exp as i64 - 1075)
    };
    let ticks: u128 = if e >= 0 {
        (m * d) << e
    } else {
        let s = (-e) as u32;
        debug_assert!(s < 100, "mintick_band: |x| too small for the u128 form");
        let num = m * d;
        let lo = num >> s;
        let r = num - (lo << s);
        let half = 1u128 << (s - 1);
        // On a tick; at or above the tie (an exact tie goes away from zero); within the band
        // below it; else down.
        if r == 0 {
            lo
        } else if r >= half || (half - r) * band_den <= (1u128 << s) {
            lo + 1
        } else {
            lo
        }
    };
    (ticks as f64 / d as f64).copysign(x)
}

/// `array.percentrank(a, i)`'s two surviving denominators (`pin:04-array-percentrank`, P7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankDen {
    /// `/ (size − 1)` — pine-std's reading.
    SizeMinusOne,
    /// `/ the non-na count`.
    NonNaCount,
}

/// `100 · #(other elements ≤ a[i]) / den` — an `na` element is never `≤` (pine-std's
/// `coll/order.rs` `percentrank` form, `(100 · count) / den`).
pub fn percentrank(a: &[f64], i: usize, den: RankDen) -> f64 {
    let r = a[i];
    if r.is_nan() {
        return NA;
    }
    let mut c = 0u32;
    for (k, &v) in a.iter().enumerate() {
        if k != i && !v.is_nan() && v <= r {
            c += 1;
        }
    }
    let d = match den {
        RankDen::SizeMinusOne => a.len() - 1,
        RankDen::NonNaCount => a.iter().filter(|v| !v.is_nan()).count(),
    };
    if d == 0 {
        NA
    } else {
        (100.0 * f64::from(c)) / d as f64
    }
}
