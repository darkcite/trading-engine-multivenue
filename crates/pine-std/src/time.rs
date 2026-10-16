// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Time builtins (impl-04 §4.12, T22): `time` / `time_close` (all three overloads), the civil
//! fields `year month dayofmonth dayofweek hour minute second` (function and variable forms),
//! `timenow`, `time_tradingday`, `timeframe.in_seconds`, `timeframe.change`. Every time is UTC
//! milliseconds; bar boundaries come from `core_time::barclock` only; no clock (`timenow` is the
//! host's event time). `weekofyear` stays a stub: no reference text defines its numbering
//! (`pin:04-weekofyear-open`).

use crate::load::{session_parse, tf_seconds, tz_lookup, tz_offset_s, SessionSpec, TzId, TZ_UTC};
use crate::state::Prev;
use core_time::barclock;
use pine_ir::{Dialect, FaultKind, SiteId};
use pine_vm::Frame;

const DAY_MS: i64 = 86_400_000;

/// `(year, month 1..=12, day 1..=31)` of day `z` since 1970-01-01 (Hinnant's `civil_from_days`,
/// integer, the full range of a day count derived from i64 ms).
#[must_use]
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days since 1970-01-01 of the civil date `(y, m, d)` (Hinnant's `days_from_civil`); `m` in
/// 1..=12, `d` in 1..=31.
#[must_use]
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400); // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Which civil field a body reports.
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum Civil {
    /// `year`.
    Year = 0,
    /// `month`, 1..=12.
    Month = 1,
    /// `dayofmonth`, 1..=31.
    DayOfMonth = 2,
    /// `dayofweek`, 1 = Sunday … 7 = Saturday (verified, time page).
    DayOfWeek = 3,
    /// `hour`, 0..=23.
    Hour = 4,
    /// `minute`, 0..=59.
    Minute = 5,
    /// `second`, 0..=59.
    Second = 6,
}

/// Field `k` of UTC ms `t` shifted by `off_s` seconds (the zone's offset at `t`).
#[inline]
fn civil_field(t: i64, off_s: i32, k: u8) -> i64 {
    let local = t.saturating_add(off_s as i64 * 1000);
    let days = local.div_euclid(DAY_MS);
    let ms = local.rem_euclid(DAY_MS);
    const Y: u8 = Civil::Year as u8;
    const MO: u8 = Civil::Month as u8;
    const D: u8 = Civil::DayOfMonth as u8;
    const W: u8 = Civil::DayOfWeek as u8;
    const H: u8 = Civil::Hour as u8;
    const MI: u8 = Civil::Minute as u8;
    match k {
        Y => civil_from_days(days).0,
        MO => civil_from_days(days).1 as i64,
        D => civil_from_days(days).2 as i64,
        // 1970-01-01 was a Thursday (5 in 1 = Sunday numbering).
        W => (days + 4).rem_euclid(7) + 1,
        H => ms / 3_600_000,
        MI => ms / 60_000 % 60,
        _ => ms / 1000 % 60,
    }
}

/// A `series` timezone string at run time: na → `None`; unknown → `Err(ScriptRefused)`
/// (`pin:04-time-edges`).
#[inline]
fn tz_of(f: &Frame<'_>, e: u16) -> Result<Option<TzId>, FaultKind> {
    match f.str_arg(e) {
        None => Ok(None),
        Some(s) => match tz_lookup(s) {
            Some(z) => Ok(Some(z)),
            None => Err(FaultKind::ScriptRefused),
        },
    }
}

/// `year(time, timezone)` … `second(time, timezone)` — window `[time, timezone, out]`;
/// `propagate` na.
pub fn bi_civil<const K: u8>(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let t = f.i64_arg(args[0]);
    let z = tz_of(f, args[1])?;
    let v = match (t, z) {
        (Some(t), Some(z)) => Some(civil_field(t, tz_offset_s(z, t), K)),
        _ => None,
    };
    f.set_i64_arg(args[2], v);
    Ok(())
}

/// The variables `year` … `second` — window `[out]`: the field of the bar's `time` in
/// `syminfo.timezone`, `"Etc/UTC"` for every venue (`pin:04-exchange-tz`).
pub fn bi_civil_var<const K: u8>(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let t = f.bar().open_ms;
    let v = civil_field(t, tz_offset_s(TZ_UTC, t), K);
    f.set_i64_arg(args[0], Some(v));
    Ok(())
}

/// `timenow` — `ctx().now_ms()`, the host's event time (never a clock).
pub fn bi_timenow(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let t = f.ctx().now_ms();
    f.set_i64_arg(args[0], Some(t));
    Ok(())
}

/// `time_tradingday` — `barclock::open_ms(bar.open_ms, 86_400)`: a 24/7 UTC venue's trading day
/// (`pin:04-exchange-tz`).
pub fn bi_time_tradingday(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let t = barclock::open_ms(f.bar().open_ms, 86_400);
    f.set_i64_arg(args[0], Some(t));
    Ok(())
}

/// A `series` timeframe string at run time, in seconds: `""` → the chart's; na, a string
/// outside the grammar, or one off the bar clock → `None` (na, §4.12).
#[inline]
fn tf_of(f: &Frame<'_>, e: u16) -> Option<u32> {
    let s = f.str_arg(e)?;
    let secs = match tf_seconds(s) {
        Ok(None) => f.meta().chart.timeframe_s,
        Ok(Some(x)) => x,
        Err(_) => return None,
    };
    if barclock::is_allowed(secs) {
        Some(secs)
    } else {
        None
    }
}

/// `timeframe.in_seconds(timeframe)` — window `[timeframe, out]`: `tf_seconds`, `""` → the
/// chart's `timeframe_s`; na or unparsable → na. The value is `tf_seconds`' (`"1M"` =
/// `MONTH_SORT_S`), not checked against the bar clock.
pub fn bi_tf_in_seconds(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let v = match f.str_arg(args[0]) {
        None => None,
        Some(s) => match tf_seconds(s) {
            Ok(None) => Some(f.meta().chart.timeframe_s as i64),
            Ok(Some(x)) => Some(x as i64),
            Err(_) => None,
        },
    };
    f.set_i64_arg(args[1], v);
    Ok(())
}

/// `timeframe.change(timeframe)` — window `[timeframe, out]`, block `Prev` (`v` = the last
/// period open as f64 bits): true when `open_ms(bar.open_ms, tf)` differs from the last
/// execution's; the first execution is true (`pin:04-time-edges`). A na timeframe (the
/// parameter's default) is the chart's; one off the bar clock or unparsable → false (a `bool`
/// result has no na in v6; `pin:04-time-edges`).
pub fn bi_tf_change(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let tf = if f.str_arg(args[0]).is_none() {
        Some(f.meta().chart.timeframe_s)
    } else {
        tf_of(f, args[0])
    };
    let open = f.bar().open_ms;
    let r = match tf {
        None => false,
        Some(tf) => {
            let p = barclock::open_ms(open, tf);
            let s = f.state::<Prev>(site);
            let changed = s.primed == 0 || s.v.to_bits() as i64 != p;
            s.v = f64::from_bits(p as u64);
            s.primed = 1;
            changed
        }
    };
    f.set_bool_arg(args[1], r);
    Ok(())
}

/// Window positions of a `time` / `time_close` overload: `(session, timezone, bars_back,
/// timeframe_bars_back)` (`u16::MAX` = absent) and the window length.
/// `K`: 0 = `TIME_I` / `TIME_CLOSE_I` (v5), 1 = `*_SSII_5_1`, 2 = `*_SSII_5_2`.
#[inline]
const fn time_layout(k: u8, d: Dialect) -> ([u16; 4], usize) {
    const NO: u16 = u16::MAX;
    let v6 = matches!(d, Dialect::V6);
    match k {
        0 => ([NO, NO, 1, NO], 3),
        1 => {
            if v6 {
                ([1, NO, 2, 3], 5)
            } else {
                ([1, NO, 2, NO], 4)
            }
        }
        _ => {
            if v6 {
                ([1, 2, 3, 4], 6)
            } else {
                ([1, 2, 3, NO], 5)
            }
        }
    }
}

/// Is local minute-of-day `m` on day-of-week `dow` (1 = Sunday) inside `sp`? An overnight range
/// (`start > end`) belongs to the day it **starts** on (`pin:04-time-edges`).
#[inline]
fn in_session(sp: &SessionSpec, m: u16, dow: u8) -> bool {
    let today = sp.days & (1 << (dow - 1)) != 0;
    let yday = sp.days & (1 << ((dow + 5) % 7)) != 0;
    let mut i = 0usize;
    while i < sp.n as usize && i < sp.ranges.len() {
        let r = sp.ranges[i];
        // `start == end` is the empty half-open range, the literal reading
        // (`pin:04-session-equal-ends`).
        let hit = if r.start_min <= r.end_min {
            today && m >= r.start_min && m < r.end_min
        } else {
            (today && m >= r.start_min) || (yday && m < r.end_min)
        };
        if hit {
            return true;
        }
        i += 1;
    }
    false
}

/// `time(timeframe[, session[, timezone]][, bars_back[, timeframe_bars_back]])` (`CLOSE` false)
/// and `time_close(…)` (`CLOSE` true): `t = barclock::open_ms(bar.open_ms, tf)`; with a session,
/// `t`'s local minute-of-day and day-of-week (in `timezone`, default `syminfo.timezone` =
/// UTC) inside the session → `t` (or `close_ms(t, tf)`), else **na** (§4.12). `""` timeframe →
/// the chart's; `""` session → the symbol's, `"24x7"` (`pin:04-exchange-tz`). A nonzero
/// `bars_back` / `timeframe_bars_back` needs the chart's own history, which a stateless body
/// does not have: `Err(ScriptRefused)` (`pin:04-time-bars-back-open`).
pub fn bi_time<const CLOSE: bool, const K: u8>(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    let ([sess, tz, bb, tfbb], n) = time_layout(K, f.dialect());
    if args.len() != n {
        return Err(FaultKind::Internal);
    }
    let out = args[n - 1];
    let mut k = 0usize;
    while k < 2 {
        let p = if k == 0 { bb } else { tfbb };
        if p != u16::MAX {
            if let Some(x) = f.i64_arg(args[p as usize]) {
                if x != 0 {
                    return Err(FaultKind::ScriptRefused);
                }
            }
        }
        k += 1;
    }
    let Some(tf) = tf_of(f, args[0]) else {
        f.set_i64_arg(out, None);
        return Ok(());
    };
    let t = barclock::open_ms(f.bar().open_ms, tf);
    if sess != u16::MAX {
        let zone = if tz == u16::MAX {
            TZ_UTC
        } else {
            match tz_of(f, args[tz as usize])? {
                Some(z) => z,
                None => {
                    f.set_i64_arg(out, None);
                    return Ok(());
                }
            }
        };
        let Some(s) = f.str_arg(args[sess as usize]) else {
            f.set_i64_arg(out, None);
            return Ok(());
        };
        if !s.is_empty() {
            let sp = session_parse(s).map_err(|_| FaultKind::ScriptRefused)?;
            let local = t.saturating_add(tz_offset_s(zone, t) as i64 * 1000);
            let m = (local.rem_euclid(DAY_MS) / 60_000) as u16;
            let dow = ((local.div_euclid(DAY_MS) + 4).rem_euclid(7) + 1) as u8;
            if !in_session(&sp, m, dow) {
                f.set_i64_arg(out, None);
                return Ok(());
            }
        }
    }
    let v = if CLOSE { barclock::close_ms(t, tf) } else { t };
    f.set_i64_arg(out, Some(v));
    Ok(())
}

/// Local civil fields → local ms since the epoch, every field carried (`month` 13 is January of
/// the next year, `day` 0 the previous month's last, `hour` 25 the next day's first, …;
/// `pin:04-time-edges`); an i64 overflow → `None`.
#[inline]
fn local_ms(fl: [i64; 6]) -> Option<i64> {
    let [y, mo, d, h, mi, s] = fl;
    let m0 = mo.checked_sub(1)?;
    let y = y.checked_add(m0.div_euclid(12))?;
    let m = (m0.rem_euclid(12) + 1) as u32;
    // `days_from_civil` is exact for |y| far beyond any i64-ms date; bound it first.
    if !(-300_000_000..=300_000_000).contains(&y) {
        return None;
    }
    let days = days_from_civil(y, m, 1).checked_add(d.checked_sub(1)?)?;
    let secs = h
        .checked_mul(3600)?
        .checked_add(mi.checked_mul(60)?)?
        .checked_add(s)?;
    days.checked_mul(DAY_MS)?
        .checked_add(secs.checked_mul(1000)?)
}

/// `timestamp(year, month, day[, hour, minute, second])` (`TZ` false, window `[y, mo, d, h, mi,
/// s, out]`, `syminfo.timezone` = UTC) and `timestamp(timezone, year, …)` (`TZ` true, window
/// `[tz, y, mo, d, h, mi, s, out]`): the local civil time → UTC ms. The zone's offset is taken
/// at `local − offset(local)` and re-read once there (a DST gap or overlap resolves to that
/// second reading, `pin:04-time-edges`). `propagate` na; overflow → na.
pub fn bi_timestamp<const TZ: bool>(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    let base = TZ as usize;
    if args.len() != base + 7 {
        return Err(FaultKind::Internal);
    }
    let out = args[base + 6];
    let zone = if TZ {
        match tz_of(f, args[0])? {
            Some(z) => z,
            None => {
                f.set_i64_arg(out, None);
                return Ok(());
            }
        }
    } else {
        TZ_UTC
    };
    let mut fl = [0i64; 6];
    let mut k = 0usize;
    while k < 6 {
        let Some(v) = f.i64_arg(args[base + k]) else {
            f.set_i64_arg(out, None);
            return Ok(());
        };
        fl[k] = v;
        k += 1;
    }
    let v = local_ms(fl).and_then(|local| {
        let o1 = tz_offset_s(zone, local) as i64 * 1000;
        let t1 = local.checked_sub(o1)?;
        let o2 = tz_offset_s(zone, t1) as i64 * 1000;
        local.checked_sub(o2)
    });
    f.set_i64_arg(out, v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `days_from_civil ∘ civil_from_days` is the identity over ±10⁶ days, and consecutive days
    /// are consecutive civil dates.
    #[test]
    fn civil_roundtrips_over_the_full_range() {
        let mut prev = civil_from_days(-1_000_001);
        let mut z = -1_000_000i64;
        while z <= 1_000_000 {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
            let next_ok = (y, m, d) == (prev.0, prev.1, prev.2 + 1)
                || (d == 1 && (m == prev.1 + 1 && y == prev.0 || m == 1 && y == prev.0 + 1));
            assert!(next_ok, "{z}: {prev:?} → {:?}", (y, m, d));
            prev = (y, m, d);
            z += 1;
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_729), (2024, 1, 7));
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
    }
}
