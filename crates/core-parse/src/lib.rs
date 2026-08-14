//! # core-parse
//!
//! Zero-allocation byte-scanner primitives over `&[u8]`.
//!
//! Every ingress parser (Polymarket WS, Binance WS, RSS, Alchemy JSON-
//! RPC) is handwritten on top of these helpers. `serde_json` and
//! friends are explicitly out of the hot path.
//!
//! The helpers are deliberately small — integer parsing, float parsing,
//! field-lookup on JSON-shaped bytes, `memchr`-based substring scans.
//! They return `Option<T>` on well-formedness failure; the hot path
//! upstream translates `None` into a `ParseError` and drops the frame.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

/// Position type — byte offset into a `&[u8]` slice.
pub type Pos = usize;

// ---------------------------------------------------------------
// Integer parsing
// ---------------------------------------------------------------

/// Parse an ASCII-digit unsigned integer starting at `pos`. Returns
/// `(value, new_pos)` on success, or `None` if there is no leading
/// digit at `pos`.
///
/// Does not allocate. Does not panic. Never touches bytes outside the
/// slice bounds.
#[inline]
pub fn scan_u64(buf: &[u8], pos: Pos) -> Option<(u64, Pos)> {
    let mut i = pos;
    let mut v: u64 = 0;
    let mut any = false;
    while i < buf.len() {
        let b = buf[i];
        if !b.is_ascii_digit() {
            break;
        }
        v = v.wrapping_mul(10).wrapping_add((b - b'0') as u64);
        i += 1;
        any = true;
    }
    if any {
        Some((v, i))
    } else {
        None
    }
}

/// Parse an ASCII-digit signed integer (optional leading `-`). Returns
/// `(value, new_pos)` on success.
#[inline]
pub fn scan_i64(buf: &[u8], pos: Pos) -> Option<(i64, Pos)> {
    if pos >= buf.len() {
        return None;
    }
    let (negative, start) = if buf[pos] == b'-' {
        (true, pos + 1)
    } else {
        (false, pos)
    };
    let (mag, next) = scan_u64(buf, start)?;
    let signed = if negative {
        -(mag as i64)
    } else {
        mag as i64
    };
    Some((signed, next))
}

// ---------------------------------------------------------------
// Float parsing -> fixed-point integer (1e-6 scale)
// ---------------------------------------------------------------

/// Parse a decimal number like `"0.518000"` and return it scaled by
/// `1e6` as `i64`. Returns `None` on malformed input.
///
/// This is the canonical price-parsing primitive used by every ingress
/// adapter. It keeps everything in integer arithmetic, so there is no
/// rounding drift across the hot path.
#[inline]
pub fn scan_price_1e6(buf: &[u8], pos: Pos) -> Option<(i64, Pos)> {
    const SCALE: u64 = 1_000_000;

    if pos >= buf.len() {
        return None;
    }
    let (negative, start) = if buf[pos] == b'-' {
        (true, pos + 1)
    } else {
        (false, pos)
    };
    let (int_part, mid) = scan_u64(buf, start)?;

    let (frac_scaled, end) = if mid < buf.len() && buf[mid] == b'.' {
        scan_fractional_1e6(buf, mid + 1)?
    } else {
        (0u64, mid)
    };

    // Overflow-safe combine. We only ever deal with prices in [0, 1], so
    // `int_part <= 1` in practice, but be correct for anything up to i64.
    let mag_i64 = int_part
        .checked_mul(SCALE)
        .and_then(|x| x.checked_add(frac_scaled))
        .and_then(|x| i64::try_from(x).ok())?;

    let signed = if negative { -mag_i64 } else { mag_i64 };
    Some((signed, end))
}

/// Scan up to six fractional digits and return their value as an
/// integer scaled to 1e-6. If fewer than six are present we pad with
/// zeros; if more, we truncate (ingress data sometimes over-specifies).
#[inline]
fn scan_fractional_1e6(buf: &[u8], pos: Pos) -> Option<(u64, Pos)> {
    let mut i = pos;
    let mut v: u64 = 0;
    let mut digits_seen = 0usize;
    while i < buf.len() {
        let b = buf[i];
        if !b.is_ascii_digit() {
            break;
        }
        if digits_seen < 6 {
            v = v.wrapping_mul(10).wrapping_add((b - b'0') as u64);
        }
        digits_seen += 1;
        i += 1;
    }
    if digits_seen == 0 {
        return None;
    }
    // Pad to 6 decimal places.
    let to_pad = 6usize.saturating_sub(digits_seen);
    let mut p = 0usize;
    while p < to_pad {
        v = v.wrapping_mul(10);
        p += 1;
    }
    Some((v, i))
}

// ---------------------------------------------------------------
// JSON-shaped field lookup (cheap + fixed-subset)
// ---------------------------------------------------------------

/// Find the byte index of a known literal field in a JSON-shaped byte
/// slice. `needle` should be the bytes of the field key _including_ the
/// trailing `":"`. Returns the position of the first byte **after**
/// the `":"`.
///
/// The implementation is `memchr`-fast; it does not parse nesting, so
/// callers must know that the key is unique in the input. For our
/// venue frames the key layout is known and unique.
#[inline]
pub fn find_field(buf: &[u8], needle: &[u8]) -> Option<Pos> {
    let mut start = 0;
    while let Some(off) = memchr::memmem::find(&buf[start..], needle) {
        let abs = start + off;
        let after = abs + needle.len();
        // Ignore false matches inside a string value: the character
        // preceding `"` must be `{`, `,` or whitespace.
        if abs == 0
            || matches!(buf[abs - 1], b'{' | b',' | b' ' | b'\t' | b'\n' | b'\r')
        {
            return Some(after);
        }
        start = after;
    }
    None
}

/// After a `find_field`, optionally skip a leading quote to land on the
/// first value byte for a string field.
#[inline]
pub fn skip_byte(buf: &[u8], pos: Pos, b: u8) -> Pos {
    if pos < buf.len() && buf[pos] == b {
        pos + 1
    } else {
        pos
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_u64_parses_simple() {
        let b = b"12345, rest";
        assert_eq!(scan_u64(b, 0), Some((12345, 5)));
    }

    #[test]
    fn scan_u64_returns_none_on_non_digit() {
        let b = b"abc";
        assert_eq!(scan_u64(b, 0), None);
    }

    #[test]
    fn scan_u64_handles_offset() {
        let b = b"abc42xyz";
        assert_eq!(scan_u64(b, 3), Some((42, 5)));
    }

    #[test]
    fn scan_i64_handles_negative() {
        let b = b"-42end";
        assert_eq!(scan_i64(b, 0), Some((-42, 3)));
    }

    #[test]
    fn scan_price_1e6_parses_integer_only() {
        let b = b"5,";
        assert_eq!(scan_price_1e6(b, 0), Some((5_000_000, 1)));
    }

    #[test]
    fn scan_price_1e6_parses_small_fraction() {
        let b = b"0.518";
        let (v, p) = scan_price_1e6(b, 0).unwrap();
        assert_eq!(v, 518_000);
        assert_eq!(p, 5);
    }

    #[test]
    fn scan_price_1e6_pads_short_fraction() {
        let b = b"0.5";
        let (v, _) = scan_price_1e6(b, 0).unwrap();
        assert_eq!(v, 500_000);
    }

    #[test]
    fn scan_price_1e6_truncates_long_fraction() {
        // 9 digits of fraction; we keep 6.
        let b = b"0.123456789";
        let (v, _) = scan_price_1e6(b, 0).unwrap();
        assert_eq!(v, 123_456);
    }

    #[test]
    fn scan_price_1e6_rejects_empty() {
        assert_eq!(scan_price_1e6(b"", 0), None);
    }

    #[test]
    fn scan_price_1e6_rejects_bare_dot() {
        assert_eq!(scan_price_1e6(b".", 0), None);
    }

    #[test]
    fn scan_price_1e6_handles_negative() {
        let (v, _) = scan_price_1e6(b"-0.5", 0).unwrap();
        assert_eq!(v, -500_000);
    }

    #[test]
    fn find_field_matches_quoted_key() {
        let b = br#"{"price":"0.5","qty":"10"}"#;
        let pos = find_field(b, br#""price":"#).unwrap();
        // Next byte should be the opening quote of the value.
        assert_eq!(b[pos], b'"');
    }

    #[test]
    fn find_field_respects_boundary() {
        // Ensure we don't match "price" inside another string value.
        let b = br#"{"note":"priceless","price":"0.5"}"#;
        let pos = find_field(b, br#""price":"#).unwrap();
        // Value immediately follows.
        let pos2 = skip_byte(b, pos, b'"');
        assert_eq!(scan_price_1e6(b, pos2), Some((500_000, pos2 + 3)));
    }
}

// ---------------------------------------------------------------
// Property tests — any byte string that looks like
// `\d+(\.\d{1,6})?` is parseable and roundtrips.
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn scan_u64_roundtrips(v in 0u64..1_000_000_000u64) {
            let mut buf = [0u8; 32];
            let s = format_u64(&mut buf, v);
            let (out, end) = scan_u64(s, 0).expect("parse");
            prop_assert_eq!(out, v);
            prop_assert_eq!(end, s.len());
        }

        #[test]
        fn scan_price_roundtrips_whole(v in 0u64..1_000_000u64) {
            // Zero-padded "0.XXXXXX" rendering of a fixed-point value.
            let mut buf = [0u8; 16];
            let s = format_price_1e6(&mut buf, v);
            let (out, _) = scan_price_1e6(s, 0).expect("parse");
            prop_assert_eq!(out, v as i64);
        }
    }

    // Tiny formatter used only inside the property test above.
    // Writes into a stack buffer and returns a &[u8] sub-slice.
    fn format_u64(buf: &mut [u8], mut v: u64) -> &[u8] {
        if v == 0 {
            buf[0] = b'0';
            return &buf[..1];
        }
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        let n = tmp.len() - i;
        buf[..n].copy_from_slice(&tmp[i..]);
        &buf[..n]
    }

    fn format_price_1e6(buf: &mut [u8], scaled: u64) -> &[u8] {
        // "<int>.<6-digit frac>"
        let int_part = scaled / 1_000_000;
        let frac_part = scaled % 1_000_000;
        let mut tmp = [0u8; 24];
        let mut n = 0;
        // int
        if int_part == 0 {
            tmp[n] = b'0';
            n += 1;
        } else {
            let mut rev = [0u8; 20];
            let mut ri = 0;
            let mut v = int_part;
            while v > 0 {
                rev[ri] = b'0' + (v % 10) as u8;
                ri += 1;
                v /= 10;
            }
            while ri > 0 {
                ri -= 1;
                tmp[n] = rev[ri];
                n += 1;
            }
        }
        tmp[n] = b'.';
        n += 1;
        // frac zero-padded to 6
        let frac_bytes = [
            b'0' + ((frac_part / 100_000) % 10) as u8,
            b'0' + ((frac_part / 10_000) % 10) as u8,
            b'0' + ((frac_part / 1_000) % 10) as u8,
            b'0' + ((frac_part / 100) % 10) as u8,
            b'0' + ((frac_part / 10) % 10) as u8,
            b'0' + (frac_part % 10) as u8,
        ];
        for b in frac_bytes {
            tmp[n] = b;
            n += 1;
        }
        buf[..n].copy_from_slice(&tmp[..n]);
        &buf[..n]
    }
}
