// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Deribit option-instrument NAME parser — `BTC-<DMMMYY>-<STRIKE>-<C|P>`.
//!
//! WHY THIS EXISTS (VRP V1; V0(d) finding). On the LIVE boot path nothing
//! parses a name: `ingress-deribit::discovery` reads `strike`,
//! `expiration_timestamp` and `option_type` straight out of the venue's
//! REST JSON, already numeric. The HARNESS has no such row — an offline
//! consumer sees only the interned descriptor from a capture run dir's
//! `instrument-manifest.tsv` (`crates/cli/src/options_manifest.rs:45`),
//! which for an option is `deribit:BTC-10SEP26-79000-C`. So the name is
//! the only place the strike, expiry and right exist offline, and this is
//! the one home for reading it.
//!
//! DOCTRINE: boot/offline path. No allocation, no `unsafe`, no deps —
//! parsing is in place over `&[u8]` and every field is returned by value
//! or as a borrowed span. It is not on a hot path, but it does not
//! allocate either, so a caller may use it anywhere.
//!
//! FAIL-CLOSED. Every malformed input returns `None`; nothing here can
//! panic on arbitrary bytes (a proptest pins that). In particular the
//! two SIBLING GRAMMARS that ship in the same manifest are rejected,
//! because feeding either to this parser would silently mint a wrong
//! expiry:
//!
//! | venue         | descriptor                        | why it is refused          |
//! |---------------|-----------------------------------|----------------------------|
//! | Deribit       | `BTC-10SEP26-79000-C`             | accepted                   |
//! | OKX           | `BTC-USD-260910-79000-C`          | 5 dash-fields, not 4       |
//! | Binance eapi  | `BTC-260910-79000-C`              | 4 fields, but `260910` has |
//! |               |                                   | no 3-letter month          |
//!
//! The month lookup is what separates Deribit from Binance-eapi: both
//! have four fields, and only Deribit's date field carries `JAN`..`DEC`.
//!
//! Deribit dailies expire at **08:00 UTC** (the expiry instant the whole
//! VRP lane is written against). The date field is `<D|DD><MMM><YY>` with
//! a 2-digit year in 2000..2099.

/// `right` byte: a call.
pub const RIGHT_CALL: u8 = 0;
/// `right` byte: a put.
pub const RIGHT_PUT: u8 = 1;

/// Deribit's daily/weekly/monthly expiry hour, UTC.
pub const EXPIRY_HOUR_UTC: u64 = 8;

const NS_PER_S: u64 = 1_000_000_000;
const S_PER_DAY: i64 = 86_400;
/// Strike is carried ×1e6, matching `OptInstrument::strike_1e6`.
const STRIKE_SCALE: i64 = 1_000_000;
/// Guard: reject a strike whose ×1e6 value would not fit an `i64`.
const STRIKE_INT_MAX: i64 = i64::MAX / STRIKE_SCALE;

/// One parsed Deribit option name. The currency is a BORROWED span into
/// the caller's bytes — zero-copy, and the caller compares it against
/// whatever it is filtering for (`b"BTC"`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ParsedOptionName<'a> {
    /// Underlying currency span, e.g. `b"BTC"`.
    pub ccy: &'a [u8],
    /// Expiry instant, ns since the unix epoch (08:00 UTC on the date).
    pub expiry_ns: u64,
    /// Strike ×1e6.
    pub strike_1e6: i64,
    /// [`RIGHT_CALL`] or [`RIGHT_PUT`].
    pub right: u8,
}

/// Strip a leading `deribit:` venue namespace if present.
///
/// The run-dir manifest writes descriptors namespaced
/// (`options_manifest.rs:113-116`); a boot-side caller holding a bare
/// `instrument_name` has no prefix. Both shapes reach the same parser
/// through [`parse_deribit_descriptor`].
#[inline]
#[must_use]
pub fn strip_deribit_prefix(desc: &[u8]) -> &[u8] {
    const P: &[u8] = b"deribit:";
    if desc.len() > P.len() && starts_with(desc, P) {
        // SAFETY-free: plain slice, bounds proven by the length test.
        &desc[P.len()..]
    } else {
        desc
    }
}

/// Parse a manifest descriptor (`deribit:BTC-10SEP26-79000-C`) or a bare
/// instrument name. Returns `None` on anything that is not a Deribit
/// option name.
#[inline]
#[must_use]
pub fn parse_deribit_descriptor(desc: &[u8]) -> Option<ParsedOptionName<'_>> {
    parse_deribit_option_name(strip_deribit_prefix(desc))
}

/// Parse a bare Deribit option instrument name.
///
/// Accepts exactly `<CCY>-<D|DD><MMM><YY>-<STRIKE>-<C|P>`. `STRIKE` is a
/// positive decimal integer with an optional fractional part of at most
/// six digits (BTC and ETH strikes are integers; the fraction is
/// accepted so a future non-integer strike is not silently truncated).
/// Deribit's `d`-for-decimal-point spelling is deliberately NOT accepted
/// — it would have to be guessed, and this parser fails closed.
#[must_use]
pub fn parse_deribit_option_name(name: &[u8]) -> Option<ParsedOptionName<'_>> {
    // Exactly four dash-separated fields. Splitting by index keeps this
    // allocation-free and lets the field count itself be a rejection.
    let (f0, f1, f2, f3) = split4(name)?;

    if f0.is_empty() {
        return None;
    }
    let right = match f3 {
        b"C" => RIGHT_CALL,
        b"P" => RIGHT_PUT,
        _ => return None,
    };
    let expiry_ns = parse_expiry_ns(f1)?;
    let strike_1e6 = parse_strike_1e6(f2)?;
    Some(ParsedOptionName {
        ccy: f0,
        expiry_ns,
        strike_1e6,
        right,
    })
}

// ---------------------------------------------------------------
// field splitting
// ---------------------------------------------------------------

/// Split into exactly four `-`-separated fields, or `None`. Five fields
/// (OKX) and three fields both fail here, which is the point.
#[inline]
fn split4(s: &[u8]) -> Option<(&[u8], &[u8], &[u8], &[u8])> {
    let mut cut = [0usize; 3];
    let mut n = 0usize;
    let mut i = 0usize;
    while i < s.len() {
        if s[i] == b'-' {
            if n == 3 {
                return None; // a fourth dash ⇒ five fields
            }
            cut[n] = i;
            n += 1;
        }
        i += 1;
    }
    if n != 3 {
        return None;
    }
    Some((
        &s[..cut[0]],
        &s[cut[0] + 1..cut[1]],
        &s[cut[1] + 1..cut[2]],
        &s[cut[2] + 1..],
    ))
}

#[inline]
fn starts_with(hay: &[u8], pre: &[u8]) -> bool {
    if hay.len() < pre.len() {
        return false;
    }
    let mut i = 0usize;
    while i < pre.len() {
        if hay[i] != pre[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ---------------------------------------------------------------
// date
// ---------------------------------------------------------------

/// `<D|DD><MMM><YY>` → ns since epoch at 08:00 UTC.
#[inline]
fn parse_expiry_ns(f: &[u8]) -> Option<u64> {
    // Day is 1 or 2 digits, so the month starts at index 1 or 2 and the
    // year is the remaining two. Total length is therefore 6 or 7.
    let (day_len, month_at) = match f.len() {
        6 => (1usize, 1usize),
        7 => (2usize, 2usize),
        _ => return None,
    };
    let day = parse_u32_fixed(&f[..day_len])?;
    let month = month_from_name(&f[month_at..month_at + 3])?;
    let yy = parse_u32_fixed(&f[month_at + 3..])?;
    let year = 2000i64 + yy as i64;
    if !valid_ymd(year, month, day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * S_PER_DAY + (EXPIRY_HOUR_UTC as i64) * 3600;
    if secs < 0 {
        return None;
    }
    Some((secs as u64) * NS_PER_S)
}

/// Uppercase 3-letter month → 1..=12.
#[inline]
fn month_from_name(m: &[u8]) -> Option<u32> {
    // Table order IS the month number. A byte-compare table rather than a
    // match on a slice pattern keeps this trivially auditable.
    const NAMES: [&[u8; 3]; 12] = [
        b"JAN", b"FEB", b"MAR", b"APR", b"MAY", b"JUN", b"JUL", b"AUG", b"SEP", b"OCT", b"NOV",
        b"DEC",
    ];
    if m.len() != 3 {
        return None;
    }
    let mut i = 0usize;
    while i < 12 {
        let n = NAMES[i];
        if m[0] == n[0] && m[1] == n[1] && m[2] == n[2] {
            return Some(i as u32 + 1);
        }
        i += 1;
    }
    None
}

/// All-digit fixed-width decimal. Rejects empty and any non-digit, so a
/// name like `BTC-1O SEP26-…` cannot slip through as a number.
#[inline]
fn parse_u32_fixed(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 9 {
        return None;
    }
    let mut v: u32 = 0;
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u32;
        i += 1;
    }
    Some(v)
}

#[inline]
const fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[inline]
const fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

#[inline]
const fn valid_ymd(y: i64, m: u32, d: u32) -> bool {
    m >= 1 && m <= 12 && d >= 1 && d <= days_in_month(y, m)
}

/// Days since 1970-01-01 for a proleptic-Gregorian date.
///
/// Howard Hinnant's `days_from_civil` (public-domain algorithm, `chrono`
/// -free by design: this crate has no dependencies). Exact for every year
/// this parser can produce (2000..=2099).
#[inline]
const fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = ((m + 9) % 12) as i64; // Mar = 0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------
// strike
// ---------------------------------------------------------------

/// Positive decimal, optional `.` fraction of ≤ 6 digits, → ×1e6.
#[inline]
fn parse_strike_1e6(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let mut int_part: i64 = 0;
    let mut i = 0usize;
    while i < s.len() && s[i] != b'.' {
        let c = s[i];
        if !c.is_ascii_digit() {
            return None;
        }
        if int_part > STRIKE_INT_MAX / 10 {
            return None; // would overflow once scaled
        }
        int_part = int_part * 10 + (c - b'0') as i64;
        i += 1;
    }
    if i == 0 {
        return None; // leading '.'
    }
    if int_part > STRIKE_INT_MAX {
        return None;
    }
    let mut frac: i64 = 0;
    let mut scale: i64 = STRIKE_SCALE;
    if i < s.len() {
        i += 1; // skip '.'
        if i == s.len() {
            return None; // trailing '.'
        }
        let mut digits = 0usize;
        while i < s.len() {
            let c = s[i];
            if !c.is_ascii_digit() {
                return None;
            }
            if digits == 6 {
                return None; // more precision than the ×1e6 field holds
            }
            frac = frac * 10 + (c - b'0') as i64;
            scale /= 10;
            digits += 1;
            i += 1;
        }
    }
    let v = int_part * STRIKE_SCALE + frac * scale;
    if v <= 0 {
        return None; // a zero or absent strike is not an option
    }
    Some(v)
}
