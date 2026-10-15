// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Timezones (impl-04 §3.7, §4.12): the generated zone table plus fixed UTC offsets.

use super::tz_table::{NAMES, ZONES};

/// A timezone: `0..0x8000` indexes the generated zone table; `0x8000 | (minutes + 1440)` is a
/// fixed UTC offset of `minutes` (|minutes| ≤ 14 h).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct TzId(pub u16);

/// `UTC` (zone 0 of the generated table).
pub const TZ_UTC: TzId = TzId(0);

const FIXED: u16 = 0x8000;
const MAX_FIXED_MIN: i32 = 14 * 60;

/// One generated zone: `off[k]` holds from `at[k]` (UTC seconds) on; `base` before `at[0]`.
pub(super) struct Zone {
    /// Offset in seconds before the first transition.
    pub(super) base: i32,
    /// Transition instants, UTC seconds, ascending.
    pub(super) at: &'static [i64],
    /// Offset in seconds from each transition on.
    pub(super) off: &'static [i32],
}

/// `"UTC"`, `"GMT"`, `"Etc/UTC"`, `"UTC±H"`, `"UTC±HH:MM"`, `"GMT±HHMM"` and the table's IANA names
/// (<https://www.tradingview.com/pine-script-docs/concepts/time/>). `None` → the caller refuses
/// 5003: a zone that silently became UTC would move every session boundary.
pub fn tz_lookup(name: &[u8]) -> Option<TzId> {
    let mut i = 0usize;
    while i < NAMES.len() {
        if NAMES[i].0 == name {
            return Some(TzId(NAMES[i].1));
        }
        i += 1;
    }
    if name.len() > 3 && (name.starts_with(b"UTC") || name.starts_with(b"GMT")) {
        return fixed_offset(&name[3..]);
    }
    None
}

/// `±H`, `±HH`, `±H:MM`, `±HH:MM`, `±HHMM` → a fixed-offset `TzId`.
fn fixed_offset(s: &[u8]) -> Option<TzId> {
    let neg = match s[0] {
        b'+' => false,
        b'-' => true,
        _ => return None,
    };
    let d = &s[1..];
    let dig = |k: usize| -> Option<i32> {
        match d.get(k) {
            Some(&c) if c.is_ascii_digit() => Some((c - b'0') as i32),
            _ => None,
        }
    };
    let (h, m) = match d.len() {
        1 => (dig(0)?, 0),
        2 => (dig(0)? * 10 + dig(1)?, 0),
        4 if d[1] == b':' => (dig(0)?, dig(2)? * 10 + dig(3)?),
        4 => (dig(0)? * 10 + dig(1)?, dig(2)? * 10 + dig(3)?),
        5 if d[2] == b':' => (dig(0)? * 10 + dig(1)?, dig(3)? * 10 + dig(4)?),
        _ => return None,
    };
    let min = h * 60 + m;
    if m > 59 || min > MAX_FIXED_MIN {
        return None;
    }
    let min = if neg { -min } else { min };
    Some(TzId(FIXED | (min + 1440) as u16))
}

/// UTC offset in seconds at `t_ms` (UTC milliseconds). An id outside the table and the fixed
/// range is a caller defect (only `tz_lookup` mints ids): it reads as UTC in release and asserts
/// in debug.
pub fn tz_offset_s(z: TzId, t_ms: i64) -> i32 {
    if z.0 & FIXED != 0 {
        return ((z.0 & !FIXED) as i32 - 1440) * 60;
    }
    let Some(zone) = ZONES.get(z.0 as usize) else {
        debug_assert!(false, "TzId {} was not minted by tz_lookup", z.0);
        return 0;
    };
    let t = t_ms.div_euclid(1000);
    // Binary search: the number of transitions at or before `t`.
    let (mut lo, mut hi) = (0usize, zone.at.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if zone.at[mid] <= t {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 {
        zone.base
    } else {
        zone.off[lo - 1]
    }
}
