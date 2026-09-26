// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Engine ×1e6 ↔ the venue's decimal strings.
//!
//! Prices (USD per contract) and sizes (contracts) travel as strings,
//! and **the signed bytes must equal the sent bytes** (D7): `"100.0"`
//! and `"100"` are different orders. One renderer therefore decides
//! the spelling — the integer part, then up to six fractional digits
//! with trailing zeros trimmed (and no `.` when none remain) — and the
//! body carries exactly what it wrote. The scanner is its inverse and
//! is EXACT: a venue string with a non-zero seventh decimal is refused,
//! never truncated into a different quantity.

/// Longest rendering of a positive `i64` at ×1e6: 13 integer digits,
/// `.`, 6 decimals.
pub const NUM_MAX: usize = 20;

/// Render `v` (×1e6, `> 0`) into `out`; the byte count, or `None` for
/// `v ≤ 0` or an `out` too short.
#[must_use]
pub fn render_1e6(v: i64, out: &mut [u8]) -> Option<usize> {
    if v <= 0 {
        return None;
    }
    let int = (v / 1_000_000) as u64;
    let mut frac = (v % 1_000_000) as u32;
    // Integer part, most significant first.
    let mut digits = [0u8; 20];
    let mut nd = 0usize;
    let mut x = int;
    loop {
        digits[nd] = b'0' + (x % 10) as u8;
        nd += 1;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    let mut fd = 6usize;
    while fd > 0 && frac % 10 == 0 {
        frac /= 10;
        fd -= 1;
    }
    let len = nd + if fd > 0 { 1 + fd } else { 0 };
    if out.len() < len {
        return None;
    }
    let mut k = 0usize;
    while k < nd {
        out[k] = digits[nd - 1 - k];
        k += 1;
    }
    if fd > 0 {
        out[nd] = b'.';
        let mut j = fd;
        let mut f = frac;
        while j > 0 {
            out[nd + j] = b'0' + (f % 10) as u8;
            f /= 10;
            j -= 1;
        }
    }
    Some(len)
}

/// The exact inverse: a non-negative decimal string (`"12"`, `"0.0523"`,
/// `"5.000000000"`) into ×1e6; `None` for anything else — a sign, an
/// exponent, an empty part, overflow, or a non-zero digit past the
/// sixth decimal.
#[must_use]
pub fn scan_1e6_exact(s: &[u8]) -> Option<i64> {
    let mut i = 0usize;
    let mut int: u64 = 0;
    let mut any = false;
    while i < s.len() && s[i].is_ascii_digit() {
        int = int.checked_mul(10)?.checked_add(u64::from(s[i] - b'0'))?;
        i += 1;
        any = true;
    }
    if !any {
        return None;
    }
    let mut frac: u64 = 0;
    let mut fd = 0usize;
    if i < s.len() {
        if s[i] != b'.' {
            return None;
        }
        i += 1;
        let start = i;
        while i < s.len() && s[i].is_ascii_digit() {
            if fd < 6 {
                frac = frac * 10 + u64::from(s[i] - b'0');
                fd += 1;
            } else if s[i] != b'0' {
                return None;
            }
            i += 1;
        }
        if i == start || i != s.len() {
            return None;
        }
    }
    while fd < 6 {
        frac *= 10;
        fd += 1;
    }
    let v = int.checked_mul(1_000_000)?.checked_add(frac)?;
    i64::try_from(v).ok()
}

/// As [`scan_1e6_exact`], with an optional leading `-` (a position's
/// signed `amount`).
#[must_use]
pub fn scan_signed_1e6_exact(s: &[u8]) -> Option<i64> {
    match s.first() {
        Some(b'-') => scan_1e6_exact(&s[1..]).map(|v| -v),
        _ => scan_1e6_exact(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: i64) -> String {
        let mut b = [0u8; NUM_MAX];
        let n = render_1e6(v, &mut b).unwrap();
        String::from_utf8(b[..n].to_vec()).unwrap()
    }

    #[test]
    fn the_spelling_is_trimmed_and_unique() {
        assert_eq!(r(500), "0.0005", "the dust price");
        assert_eq!(r(1), "0.000001", "the dust size");
        assert_eq!(r(1_000_000), "1");
        assert_eq!(r(100_000_000), "100");
        assert_eq!(r(52_300), "0.0523");
        assert_eq!(r(7_730_500_000), "7730.5");
        assert_eq!(r(i64::MAX), "9223372036854.775807");
        assert_eq!(r(i64::MAX).len(), NUM_MAX);
    }

    #[test]
    fn nothing_non_positive_renders_and_a_short_buffer_refuses() {
        let mut b = [0u8; NUM_MAX];
        assert_eq!(render_1e6(0, &mut b), None);
        assert_eq!(render_1e6(-5, &mut b), None);
        assert_eq!(render_1e6(1_234_567, &mut b[..7]), None);
        assert_eq!(render_1e6(1_234_567, &mut b[..8]), Some(8));
    }

    #[test]
    fn the_scanner_is_the_exact_inverse() {
        let vals = [1i64, 500, 52_300, 1_000_000, 7_730_500_000, i64::MAX];
        for v in vals {
            assert_eq!(scan_1e6_exact(r(v).as_bytes()), Some(v), "{v}");
        }
        assert_eq!(scan_1e6_exact(b"5.0"), Some(5_000_000));
        assert_eq!(scan_1e6_exact(b"5.000000000"), Some(5_000_000));
        assert_eq!(scan_1e6_exact(b"0.0000005"), None, "a seventh digit is refused");
        for bad in [&b""[..], b".5", b"5.", b"-1", b"1e3", b"1.2.3", b" 1", b"99999999999999"] {
            assert_eq!(scan_1e6_exact(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        assert_eq!(scan_signed_1e6_exact(b"-0.75"), Some(-750_000));
        assert_eq!(scan_signed_1e6_exact(b"2"), Some(2_000_000));
    }
}
