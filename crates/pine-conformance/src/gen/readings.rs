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
