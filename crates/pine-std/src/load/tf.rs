// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Timeframes (impl-04 §3.7, §4.12): `tf_seconds`, `tf_string`, `tf_parts`, `tf_on_bar_clock`.

use super::{code, err, LoadErr};
use crate::sw;
use pine_ir::Dialect;
use pine_spec::{swr, Sw};

/// Seconds of one `"1M"` step, a **sort key** only (equals `core_time::barclock::TF_MONTH_S`);
/// month arithmetic is calendar-exact.
pub const MONTH_SORT_S: u32 = 2_629_746;
const _: () = assert!(MONTH_SORT_S == core_time::barclock::TF_MONTH_S);

const DAY_S: u32 = 86_400;
const WEEK_S: u32 = 604_800;

/// The `tf_multiplier` readings, folded for both dialects (impl-00 row MG-16).
const TF_MULT: [u8; 2] = sw::both(Sw::TfMultiplier);

/// A timeframe unit, as `tf_parts` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TfUnit {
    /// `S`.
    Second = 0,
    /// A bare number (minutes).
    Minute = 1,
    /// `D`.
    Day = 2,
    /// `W`.
    Week = 3,
    /// `M`.
    Month = 4,
}

/// `timeframe.multiplier` and the `timeframe.is*` predicates, for sema's folding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TfParts {
    /// The unit.
    pub unit: TfUnit,
    /// The multiplier (`timeframe.multiplier`).
    pub mult: u32,
}

#[inline]
const fn sec_mult_ok(m: u32) -> bool {
    matches!(m, 1 | 5 | 10 | 15 | 30 | 45)
}

/// Parse a Pine timeframe string. `""` → `Ok(None)` (the chart's timeframe).
///
/// Grammar (<https://www.tradingview.com/pine-script-docs/concepts/timeframes/>): a bare number
/// is minutes `1..=1440`; `S` seconds with multiplier 1, 5, 10, 15, 30 or 45; `D` 1..=365; `W`
/// 1..=52; `M` 1..=12 (× `MONTH_SORT_S`); a unit without a multiplier means 1; there is no `H`
/// unit. Refused (5001): tick timeframes (`T`), lower-case units, leading zeros, anything else.
pub fn tf_seconds(s: &[u8]) -> Result<Option<u32>, LoadErr> {
    let n = s.len();
    if n == 0 {
        return Ok(None);
    }
    if s[0] == b'0' {
        return Err(err(
            code::BAD_TIMEFRAME,
            0,
            "a timeframe multiplier has no leading zero",
        ));
    }
    let mut i = 0usize;
    let mut m: u32 = 0;
    while i < n && s[i].is_ascii_digit() {
        m = m.saturating_mul(10).saturating_add((s[i] - b'0') as u32);
        i += 1;
    }
    if i == n {
        return if (1..=1440).contains(&m) {
            Ok(Some(m * 60))
        } else {
            Err(err(
                code::BAD_TIMEFRAME,
                0,
                "a minutes timeframe is 1..=1440",
            ))
        };
    }
    if i + 1 != n {
        return Err(err(
            code::BAD_TIMEFRAME,
            i + 1,
            "characters after the timeframe unit",
        ));
    }
    let mult = if i > 0 { m } else { 1 };
    match s[i] {
        b'S' if sec_mult_ok(mult) => Ok(Some(mult)),
        b'S' => Err(err(
            code::BAD_TIMEFRAME,
            0,
            "a seconds multiplier is 1, 5, 10, 15, 30 or 45",
        )),
        b'D' if (1..=365).contains(&mult) => Ok(Some(mult * DAY_S)),
        b'W' if (1..=52).contains(&mult) => Ok(Some(mult * WEEK_S)),
        b'M' if (1..=12).contains(&mult) => Ok(Some(mult * MONTH_SORT_S)),
        b'D' | b'W' | b'M' => Err(err(
            code::BAD_TIMEFRAME,
            0,
            "timeframe multiplier out of range",
        )),
        b'T' => Err(err(
            code::BAD_TIMEFRAME,
            i,
            "tick timeframes are not supported",
        )),
        _ => Err(err(code::BAD_TIMEFRAME, i, "unknown timeframe unit")),
    }
}

/// Decompose `secs` into unit and multiplier: month, week, day, minute, second, in that order.
pub fn tf_parts(secs: u32) -> Option<TfParts> {
    let (unit, mult) = if secs == 0 {
        return None;
    } else if secs % MONTH_SORT_S == 0 && secs / MONTH_SORT_S <= 12 {
        (TfUnit::Month, secs / MONTH_SORT_S)
    } else if secs % WEEK_S == 0 && secs / WEEK_S <= 52 {
        (TfUnit::Week, secs / WEEK_S)
    } else if secs % DAY_S == 0 && secs / DAY_S <= 365 {
        (TfUnit::Day, secs / DAY_S)
    } else if secs % 60 == 0 && secs / 60 <= 1440 {
        (TfUnit::Minute, secs / 60)
    } else if sec_mult_ok(secs) {
        (TfUnit::Second, secs)
    } else {
        return None;
    };
    Some(TfParts { unit, mult })
}

/// Render `secs` as `timeframe.period` does in `dialect`, returning the bytes written (0 when
/// `secs` is no timeframe). Whether a multiplier of 1 is written for `S`/`D`/`W`/`M` is the
/// `tf_multiplier` switch (`omit_one` → `"D"`, `always` → `"1D"`), never a hardcoded rule.
pub fn tf_string(secs: u32, dialect: Dialect, out: &mut [u8; 8]) -> usize {
    let Some(p) = tf_parts(secs) else { return 0 };
    let unit = match p.unit {
        TfUnit::Second => b'S',
        TfUnit::Minute => 0,
        TfUnit::Day => b'D',
        TfUnit::Week => b'W',
        TfUnit::Month => b'M',
    };
    let omit =
        unit != 0 && p.mult == 1 && sw::pick(TF_MULT, dialect) == swr::TF_MULTIPLIER_OMIT_ONE;
    let mut n = 0usize;
    if !omit {
        let mut digits = [0u8; 4];
        let mut k = 0usize;
        let mut v = p.mult;
        loop {
            digits[k] = b'0' + (v % 10) as u8;
            k += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        while k > 0 {
            k -= 1;
            out[n] = digits[k];
            n += 1;
        }
    }
    if unit != 0 {
        out[n] = unit;
        n += 1;
    }
    n
}

/// `Ok(secs)` iff `core_time::barclock::is_allowed(secs)` — a timeframe hosts can build bars on
/// (request targets, `time(tf)` alignment); otherwise 5001.
pub fn tf_on_bar_clock(secs: u32) -> Result<u32, LoadErr> {
    if core_time::barclock::is_allowed(secs) {
        Ok(secs)
    } else {
        Err(err(
            code::BAD_TIMEFRAME,
            0,
            "no bar clock for this timeframe",
        ))
    }
}
