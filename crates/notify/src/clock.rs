// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! UTC calendar arithmetic without a time crate: Howard Hinnant's civil
//! date algorithms, RFC 3339 `Z` parsing and the few spellings messages use.
//! Every time a message prints is UTC (R-NT-8).

use std::time::{SystemTime, UNIX_EPOCH};

/// Wall clock, ns since the Unix epoch.
#[must_use]
pub fn now_wall_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
#[must_use]
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number from [`days_from_civil`].
#[must_use]
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn digits(s: &[u8]) -> Option<i64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut v = 0i64;
    for &c in s {
        v = v * 10 + i64::from(c - b'0');
    }
    Some(v)
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` or `YYYY-MM-DDTHH:MMZ` (UTC only) into
/// seconds since the epoch.
#[must_use]
pub fn parse_rfc3339_z(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20 && b.len() != 17 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[b.len() - 1] != b'Z' {
        return None;
    }
    let y = digits(&b[0..4])?;
    let mo = digits(&b[5..7])?;
    let d = digits(&b[8..10])?;
    let h = digits(&b[11..13])?;
    let mi = digits(&b[14..16])?;
    let sec = if b.len() == 20 {
        if b[16] != b':' {
            return None;
        }
        digits(&b[17..19])?
    } else {
        0
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    Some(days * 86_400 + h * 3600 + mi * 60 + sec)
}

fn split(ts_s: i64) -> (i64, u32, u32, i64, i64, i64) {
    let days = ts_s.div_euclid(86_400);
    let rem = ts_s.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// `08:45Z`.
#[must_use]
pub fn hhmm(ts_s: i64) -> String {
    let (_, _, _, h, mi, _) = split(ts_s);
    format!("{h:02}:{mi:02}Z")
}

/// `08:45:07Z`.
#[must_use]
pub fn hhmmss(ts_s: i64) -> String {
    let (_, _, _, h, mi, s) = split(ts_s);
    format!("{h:02}:{mi:02}:{s:02}Z")
}

/// `2026-10-24`.
#[must_use]
pub fn date(ts_s: i64) -> String {
    let (y, m, d, _, _, _) = split(ts_s);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `10-24 08:45Z`.
#[must_use]
pub fn mmdd_hhmm(ts_s: i64) -> String {
    let (_, m, d, h, mi, _) = split(ts_s);
    format!("{m:02}-{d:02} {h:02}:{mi:02}Z")
}

/// The UTC hour of `ts_s` (0–23).
#[must_use]
pub fn hour_of(ts_s: i64) -> u8 {
    (ts_s.rem_euclid(86_400) / 3600) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_across_eras() {
        for z in [-800_000i64, -1, 0, 1, 19_000, 20_385, 2_000_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn rfc3339_parses_both_forms_and_refuses_the_rest() {
        let t = parse_rfc3339_z("2026-10-24T05:19:25Z").unwrap();
        assert_eq!(hhmmss(t), "05:19:25Z");
        assert_eq!(date(t), "2026-10-24");
        assert_eq!(parse_rfc3339_z("2026-10-24T05:19Z"), Some(t - 25));
        assert_eq!(parse_rfc3339_z("2026-10-24 05:19:25Z"), None);
        assert_eq!(parse_rfc3339_z("2026-10-24T05:19:25+07:00"), None);
        assert_eq!(parse_rfc3339_z("2026-13-24T05:19:25Z"), None);
        assert_eq!(parse_rfc3339_z(""), None);
    }

    #[test]
    fn spellings() {
        let t = parse_rfc3339_z("2027-04-18T07:14:00Z").unwrap();
        assert_eq!(hhmm(t), "07:14Z");
        assert_eq!(mmdd_hhmm(t), "04-18 07:14Z");
        assert_eq!(hour_of(t), 7);
    }
}
