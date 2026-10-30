// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Sessions (impl-04 §3.7, §4.12): `"HHmm-HHmm[,HHmm-HHmm…][:digits]"` or `"24x7"`.

use super::{code, err, LoadErr};

/// One range, minutes from local midnight; `start_min > end_min` wraps midnight.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SessionRange {
    /// Start, minutes from local midnight.
    pub start_min: u16,
    /// End, minutes from local midnight (exclusive).
    pub end_min: u16,
}

/// A parsed session string.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SessionSpec {
    /// The ranges, `n` of them.
    pub ranges: [SessionRange; 4],
    /// Number of ranges in use.
    pub n: u8,
    /// bit 0 = Sunday … bit 6 = Saturday (Pine's day digits 1..7); 0x7F when the string has none.
    pub days: u8,
    /// Always zero.
    pub _pad: [u8; 6],
}
const _: () = assert!(core::mem::size_of::<SessionRange>() == 4);
const _: () = assert!(core::mem::align_of::<SessionRange>() == 2);
const _: () = assert!(core::mem::size_of::<SessionSpec>() == 24);
const _: () = assert!(core::mem::align_of::<SessionSpec>() == 2);

/// Parse a session (<https://www.tradingview.com/pine-script-docs/concepts/sessions/>): `HH`
/// 00–23, `mm` 00–59, day digits `1`..`7` = Sunday..Saturday. At most 4 ranges — a pine-std
/// capacity, not a Pine rule: a fifth is 5002, never a silent truncation (`pin:04-time-edges`).
pub fn session_parse(s: &[u8]) -> Result<SessionSpec, LoadErr> {
    let mut sp = SessionSpec {
        days: 0x7F,
        ..SessionSpec::default()
    };
    if s == b"24x7" {
        sp.n = 1;
        sp.ranges[0] = SessionRange {
            start_min: 0,
            end_min: 1440,
        };
        return Ok(sp);
    }
    let mut i = 0usize;
    loop {
        if sp.n as usize == sp.ranges.len() {
            return Err(err(code::BAD_SESSION, i, "more than 4 session ranges"));
        }
        let a = hhmm(s, i)?;
        if s.get(i + 4) != Some(&b'-') {
            return Err(err(
                code::BAD_SESSION,
                i + 4,
                "expected '-' between session times",
            ));
        }
        let b = hhmm(s, i + 5)?;
        sp.ranges[sp.n as usize] = SessionRange {
            start_min: a,
            end_min: b,
        };
        sp.n += 1;
        i += 9;
        match s.get(i) {
            None => return Ok(sp),
            Some(b',') => i += 1,
            Some(b':') => {
                i += 1;
                break;
            }
            Some(_) => return Err(err(code::BAD_SESSION, i, "expected ',' or ':'")),
        }
    }
    if i == s.len() {
        return Err(err(code::BAD_SESSION, i, "empty session day list"));
    }
    let mut days = 0u8;
    while i < s.len() {
        let c = s[i];
        if !(b'1'..=b'7').contains(&c) {
            return Err(err(code::BAD_SESSION, i, "a session day is a digit 1..7"));
        }
        days |= 1 << (c - b'1');
        i += 1;
    }
    sp.days = days;
    Ok(sp)
}

fn digit(s: &[u8], i: usize) -> Result<u16, LoadErr> {
    match s.get(i) {
        Some(&c) if c.is_ascii_digit() => Ok((c - b'0') as u16),
        _ => Err(err(code::BAD_SESSION, i, "expected a digit of HHmm")),
    }
}

fn hhmm(s: &[u8], i: usize) -> Result<u16, LoadErr> {
    let hh = digit(s, i)? * 10 + digit(s, i + 1)?;
    let mm = digit(s, i + 2)? * 10 + digit(s, i + 3)?;
    if hh > 23 || mm > 59 {
        return Err(err(code::BAD_SESSION, i, "session time out of range"));
    }
    Ok(hh * 60 + mm)
}
