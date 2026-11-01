// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Decimal entry → ×1e6 integer (T-L8). More than 6 decimals is refused,
//! never rounded. The same routine runs in the WASM (feedback) and on the
//! server (authority).

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecError {
    Empty,
    BadChar,
    TooManyDecimals,
    Overflow,
}

pub const SCALE: i64 = 1_000_000;

/// Parse `[-]digits[.digits]` (ASCII, no exponent, no grouping, no spaces).
pub fn parse_1e6(s: &[u8]) -> Result<i64, DecError> {
    let (neg, s) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    if s.is_empty() {
        return Err(DecError::Empty);
    }
    let mut int: i64 = 0;
    let mut frac: i64 = 0;
    let mut nfrac: u32 = 0;
    let mut seen_dot = false;
    let mut ndig = 0usize;
    for &c in s {
        match c {
            b'.' if !seen_dot => seen_dot = true,
            b'0'..=b'9' => {
                let d = (c - b'0') as i64;
                ndig += 1;
                if seen_dot {
                    nfrac += 1;
                    if nfrac > 6 {
                        return Err(DecError::TooManyDecimals);
                    }
                    frac = frac * 10 + d;
                } else {
                    int = int
                        .checked_mul(10)
                        .and_then(|v| v.checked_add(d))
                        .ok_or(DecError::Overflow)?;
                }
            }
            _ => return Err(DecError::BadChar),
        }
    }
    if ndig == 0 {
        return Err(DecError::Empty);
    }
    let frac = frac * 10i64.pow(6 - nfrac);
    let v = int
        .checked_mul(SCALE)
        .and_then(|v| v.checked_add(frac))
        .ok_or(DecError::Overflow)?;
    Ok(if neg { -v } else { v })
}

/// Format a ×1e6 integer with exactly `decimals` (≤ 6) places, truncating
/// toward zero, into `out`. Integer arithmetic only (no f64 `Display`).
/// Returns the number of bytes written; `out` must hold 28 bytes.
pub fn format_1e6(v: i64, decimals: u32, out: &mut [u8; 28]) -> usize {
    let decimals = decimals.min(6);
    let neg = v < 0;
    let a = v.unsigned_abs();
    let int = a / SCALE as u64;
    let frac = (a % SCALE as u64) / 10u64.pow(6 - decimals);
    let mut tmp = [0u8; 28];
    let mut n = 0;
    let mut f = frac;
    for _ in 0..decimals {
        tmp[n] = b'0' + (f % 10) as u8;
        f /= 10;
        n += 1;
    }
    if decimals > 0 {
        tmp[n] = b'.';
        n += 1;
    }
    let mut i = int;
    loop {
        tmp[n] = b'0' + (i % 10) as u8;
        i /= 10;
        n += 1;
        if i == 0 {
            break;
        }
    }
    if neg && (int != 0 || frac != 0) {
        tmp[n] = b'-';
        n += 1;
    }
    for k in 0..n {
        out[k] = tmp[n - 1 - k];
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(parse_1e6(b"1"), Ok(1_000_000));
        assert_eq!(parse_1e6(b"0.000001"), Ok(1));
        assert_eq!(parse_1e6(b"-12.5"), Ok(-12_500_000));
        assert_eq!(parse_1e6(b"1."), Ok(1_000_000));
        assert_eq!(parse_1e6(b".5"), Ok(500_000));
    }

    #[test]
    fn refuses() {
        assert_eq!(parse_1e6(b"0.0000001"), Err(DecError::TooManyDecimals));
        assert_eq!(parse_1e6(b""), Err(DecError::Empty));
        assert_eq!(parse_1e6(b"-"), Err(DecError::Empty));
        assert_eq!(parse_1e6(b"."), Err(DecError::Empty));
        assert_eq!(parse_1e6(b"1e3"), Err(DecError::BadChar));
        assert_eq!(parse_1e6(b"1.2.3"), Err(DecError::BadChar));
        assert_eq!(parse_1e6(b" 1"), Err(DecError::BadChar));
        assert_eq!(parse_1e6(b"99999999999999999999"), Err(DecError::Overflow));
    }

    #[test]
    fn formats() {
        let mut b = [0u8; 28];
        let n = format_1e6(-12_345_678, 2, &mut b);
        assert_eq!(&b[..n], b"-12.34");
        let n = format_1e6(67_000_000_000, 0, &mut b);
        assert_eq!(&b[..n], b"67000");
        let n = format_1e6(-1, 2, &mut b);
        assert_eq!(&b[..n], b"0.00");
        let n = format_1e6(i64::MIN, 6, &mut b);
        assert_eq!(&b[..n], b"-9223372036854.775808");
    }

    #[test]
    fn roundtrip_6dp() {
        let mut b = [0u8; 28];
        for v in [0i64, 1, -1, 123_456_789, -987_654_321_012, i64::MAX] {
            let n = format_1e6(v, 6, &mut b);
            assert_eq!(parse_1e6(&b[..n]), Ok(v), "{v}");
        }
    }
}
