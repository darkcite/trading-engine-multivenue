// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `core_time::barclock` — the bar-boundary convention (PVM contract v4 §3.15.1)
//!
//! Pure functions on **UTC milliseconds**. No clock, no state, no allocation. Every host that
//! builds or aligns a bar — the `pine-screen` bar-file preparer, the `strategy-pine` live bar
//! builder, every sub-program stepper, `pine-std`'s `time(tf)` builtins — calls these and
//! nothing else.
//!
//! * Sub-week timeframes are **epoch-aligned**: the same grid as [`crate::BarClock`] (a test
//!   pins the agreement).
//! * `604800` is **Monday-anchored** at [`WEEK_ANCHOR_MS`] (1970-01-01 was a Thursday).
//! * [`TF_MONTH_S`] is a **calendar month**: it opens at 00:00 UTC on day 1. The constant is a
//!   sort key only; the arithmetic is calendar-exact.
//! * Bars are **left-closed, right-open**; the floor is toward −∞.
//!
//! Domain: `|t_ms| ≤ T_ABS_MAX_MS` (2^62 ms, ±146 million years), debug-asserted. Outside it
//! every function still returns (saturating arithmetic) and never panics.

/// Monday 1970-01-05T00:00:00Z in UTC milliseconds (review S4: **ms**, not the ×1000-too-large
/// `345_600_000_000_000_000` of impl-07 v1).
pub const WEEK_ANCHOR_MS: i64 = 345_600_000;

/// `timeframe = "1M"`: 30.436875 days. A **sort key only** — month arithmetic is calendar-exact.
pub const TF_MONTH_S: u32 = 2_629_746;

/// Every timeframe a host may build bars on, ascending by length. Each below a day divides a
/// day exactly.
pub const ALLOWED_TF_S: [u32; 13] = [
    60, 180, 300, 900, 1800, 3600, 7200, 14400, 21600, 43200, 86400, 604800, TF_MONTH_S,
];

const DAY_MS: i64 = 86_400_000;
const WEEK_MS: i64 = 604_800_000;
const WEEK_S: u32 = 604_800;
const T_ABS_MAX_MS: i64 = 1 << 62;

/// Whether `tf_s` is one of [`ALLOWED_TF_S`]. Anything else is a load-time refusal at the
/// caller, never a best-effort bucket.
#[inline]
#[must_use]
pub const fn is_allowed(tf_s: u32) -> bool {
    let mut i = 0usize;
    while i < ALLOWED_TF_S.len() {
        if ALLOWED_TF_S[i] == tf_s {
            return true;
        }
        i += 1;
    }
    false
}

/// The open (UTC ms) of the `tf_s` bar containing `t_ms`: left-closed, right-open, floor toward
/// −∞ (a pre-epoch instant never rounds toward zero).
///
/// `tf_s` must satisfy [`is_allowed`] (debug-asserted; callers verified it at load). An
/// unlisted non-zero `tf_s` is treated as an epoch-aligned grid of `tf_s` seconds, and `0` as
/// one millisecond — total, deterministic, never a panic.
#[inline]
#[must_use]
pub fn open_ms(t_ms: i64, tf_s: u32) -> i64 {
    debug_assert!(is_allowed(tf_s), "barclock: timeframe not allowed");
    debug_assert!(
        t_ms.unsigned_abs() <= T_ABS_MAX_MS as u64,
        "barclock: t_ms outside the domain"
    );
    if tf_s == TF_MONTH_S {
        let days = t_ms.div_euclid(DAY_MS);
        let (y, m, _d) = civil_from_days(days);
        return days_from_civil(y, m, 1).saturating_mul(DAY_MS);
    }
    if tf_s == WEEK_S {
        let off = t_ms.saturating_sub(WEEK_ANCHOR_MS).rem_euclid(WEEK_MS);
        return t_ms.saturating_sub(off);
    }
    let len = if tf_s == 0 { 1 } else { tf_s as i64 * 1_000 };
    t_ms.saturating_sub(t_ms.rem_euclid(len))
}

/// The exclusive end (UTC ms) of the bar that opens at `open_ms` — the next bar's open, and
/// what `time_close` means inside a script. `open_ms` must be a bar open for `tf_s`
/// (debug-asserted).
#[inline]
#[must_use]
pub fn close_ms(open_ms: i64, tf_s: u32) -> i64 {
    debug_assert!(
        self::open_ms(open_ms, tf_s) == open_ms,
        "barclock: close_ms of a non-open"
    );
    if tf_s == TF_MONTH_S {
        let (y, m, _d) = civil_from_days(open_ms.div_euclid(DAY_MS));
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        return days_from_civil(ny, nm, 1).saturating_mul(DAY_MS);
    }
    if tf_s == WEEK_S {
        return open_ms.saturating_add(WEEK_MS);
    }
    let len = if tf_s == 0 { 1 } else { tf_s as i64 * 1_000 };
    open_ms.saturating_add(len)
}

/// The number of `tf_s` bars that open in `[a_open_ms, b_open_ms)`; negative when `b < a`;
/// `bars_between(x, x, tf) == 0`. Both arguments must be bar opens (debug-asserted). A hole of
/// `k` missing bars between two present bars at `a` and `b` gives `k + 1`.
#[inline]
#[must_use]
pub fn bars_between(a_open_ms: i64, b_open_ms: i64, tf_s: u32) -> i64 {
    debug_assert!(
        open_ms(a_open_ms, tf_s) == a_open_ms,
        "barclock: bars_between of a non-open"
    );
    debug_assert!(
        open_ms(b_open_ms, tf_s) == b_open_ms,
        "barclock: bars_between of a non-open"
    );
    if tf_s == TF_MONTH_S {
        return month_index(b_open_ms).saturating_sub(month_index(a_open_ms));
    }
    let len = if tf_s == WEEK_S {
        WEEK_MS
    } else if tf_s == 0 {
        1
    } else {
        tf_s as i64 * 1_000
    };
    b_open_ms.saturating_sub(a_open_ms).div_euclid(len)
}

/// Months since 0000-03 (any fixed origin works: only differences are used).
#[inline]
fn month_index(t_ms: i64) -> i64 {
    let (y, m, _d) = civil_from_days(t_ms.div_euclid(DAY_MS));
    y * 12 + (m - 1)
}

/// Days since 1970-01-01 of the proleptic-Gregorian civil date `(y, m, d)` (H. Hinnant,
/// `days_from_civil`; exact over the whole domain).
#[inline]
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The civil date `(y, m, d)` of day `z` since 1970-01-01 (H. Hinnant, `civil_from_days`).
#[inline]
const fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}
