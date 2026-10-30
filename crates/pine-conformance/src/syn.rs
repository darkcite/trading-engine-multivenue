// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! SYN-1 / SYN-T — the synthetic drivers (impl-06 §4.1.1–§4.1.3) and their Rust twin `SynSeq`.
//!
//! The prelude texts are load-time constants (SYN-T is composed once per fixture, allocating).
//! `SynSeq` is `Copy`, allocation-free and does exactly the prelude's binary64 operations in the
//! prelude's order, so both sides produce the same bits (`syn_is_exact_over_32768_steps`).

/// The SYN-1 prelude, verbatim (§4.1.1). Reads no bar field and depends on no pin.
pub const SYN1_PRELUDE: &str = "\
// --- SYN-1 --- generated; do not edit. Every intermediate < 2^31, so every operation below is
// exact in binary64 and the series is bit-identical on any IEEE-754 engine.
var float _s = 1.0
var int   _n = 0
_n := _n + 1
_hi = math.floor(_s / 127773.0)
_lo = _s - _hi * 127773.0
_t  = 16807.0 * _lo - 2836.0 * _hi
_s := _t > 0.0 ? _t : _t + 2147483647.0
syn   = 1000.0 + (_s - math.floor(_s / 100000.0) * 100000.0) / 100.0
syni  = int(_s - math.floor(_s / 1000.0) * 1000.0)
synb  = syni >= 500
synna = syni < 100 ? na : syn
";

/// The SYN-T prelude with `FROM_MS` / `STOP_MS` as placeholders (§4.1.3); see [`syn_t_prelude`].
pub const SYNT_TEMPLATE: &str = "\
// --- SYN-T --- generated; do not edit. SYN-1, advanced only inside the chart window.
var float _s = 1.0
var int   _n = 0
_on   = time >= FROM_MS and time < STOP_MS
_last = time == STOP_MS
if _on
    _n := _n + 1
    _hi = math.floor(_s / 127773.0)
    _lo = _s - _hi * 127773.0
    _t  = 16807.0 * _lo - 2836.0 * _hi
    _s := _t > 0.0 ? _t : _t + 2147483647.0
syni = int(_s - math.floor(_s / 1000.0) * 1000.0)
synb = syni >= 500
";

/// SYN-T with `FROM_MS` and `STOP_MS` (`= TO_MS − 2·TF_MS`) emitted as integer literals (from
/// `pine_spec::chart` once T4 lands). Load-time; allocates.
///
/// `pin:06-synt-literal-substitution` — each placeholder token is replaced verbatim by the
/// decimal integer (no separators, no suffix); nothing else in the template changes.
pub fn syn_t_prelude(from_ms: i64, stop_ms: i64) -> String {
    SYNT_TEMPLATE
        .replace("FROM_MS", &from_ms.to_string())
        .replace("STOP_MS", &stop_ms.to_string())
}

/// MINSTD modulus `2^31 − 1`.
pub const MINSTD_M: u64 = 2_147_483_647;
/// MINSTD multiplier.
pub const MINSTD_A: u64 = 16_807;

/// One SYN-1 step: the prelude's variables after the `n`-th evaluation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SynStep {
    /// `_n` (1-based step counter).
    pub n: i64,
    /// `_s`, the MINSTD state.
    pub s: f64,
    /// `syn = 1000 + (s mod 100000) / 100`.
    pub syn: f64,
    /// `syni = int(s mod 1000)`.
    pub syni: i64,
    /// `synb = syni >= 500`.
    pub synb: bool,
    /// `synna = syni < 100 ? na : syn` (`None` = `na`).
    pub synna: Option<f64>,
}

/// The Rust twin of SYN-1: Lehmer MINSTD in Schrage form, `s₀ = 1`, the prelude's exact f64 order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SynSeq {
    s: f64,
    n: i64,
}

impl Default for SynSeq {
    fn default() -> Self {
        Self::new()
    }
}

impl SynSeq {
    /// Before the first evaluation (`_s = 1.0`, `_n = 0`).
    pub const fn new() -> Self {
        Self { s: 1.0, n: 0 }
    }

    /// The state `_s` before the next step.
    pub const fn state(&self) -> f64 {
        self.s
    }

    /// Advance one evaluation (SYN-1's body; SYN-T's `if _on` body plus its tail).
    #[inline]
    pub fn step(&mut self) -> SynStep {
        self.n += 1;
        let s = self.s;
        let hi = (s / 127773.0).floor();
        let lo = s - hi * 127773.0;
        let t = 16807.0 * lo - 2836.0 * hi;
        self.s = if t > 0.0 { t } else { t + 2147483647.0 };
        let s = self.s;
        let syn = 1000.0 + (s - (s / 100000.0).floor() * 100000.0) / 100.0;
        // `int(…)` of an exactly integral float in [0, 1000): truncation is exact.
        let syni = (s - (s / 1000.0).floor() * 1000.0) as i64;
        let synb = syni >= 500;
        let synna = if syni < 100 { None } else { Some(syn) };
        SynStep {
            n: self.n,
            s,
            syn,
            syni,
            synb,
            synna,
        }
    }
}
