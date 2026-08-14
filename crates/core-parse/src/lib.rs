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

/// Parse a decimal number like `"0.0000593"` and return it scaled by
/// `1e9` as `i64`. Same contract as [`scan_price_1e6`]. Added in
/// Phase 8b for venue *rate* fields (OKX `fundingRate` pushes values
/// like `0.0000593` whose resolution exceeds 1e-6 — at 1e6 scale
/// they'd truncate to ~1 count of precision).
#[inline]
pub fn scan_price_1e9(buf: &[u8], pos: Pos) -> Option<(i64, Pos)> {
    const SCALE: u64 = 1_000_000_000;

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
        scan_fractional_n(buf, mid + 1, 9)?
    } else {
        (0u64, mid)
    };

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
    scan_fractional_n(buf, pos, 6)
}

/// Scan up to `max_digits` fractional digits and return their value
/// as an integer scaled to `10^-max_digits`. Shorter fractions are
/// zero-padded; longer ones truncated. Shared body of the 1e6 / 1e9
/// scanners — one implementation, two scales.
#[inline]
fn scan_fractional_n(buf: &[u8], pos: Pos, max_digits: usize) -> Option<(u64, Pos)> {
    debug_assert!(max_digits <= 18, "u64 fractional overflow guard");
    let mut i = pos;
    let mut v: u64 = 0;
    let mut digits_seen = 0usize;
    while i < buf.len() {
        let b = buf[i];
        if !b.is_ascii_digit() {
            break;
        }
        if digits_seen < max_digits {
            v = v.wrapping_mul(10).wrapping_add((b - b'0') as u64);
        }
        digits_seen += 1;
        i += 1;
    }
    if digits_seen == 0 {
        return None;
    }
    // Pad to `max_digits` decimal places.
    let to_pad = max_digits.saturating_sub(digits_seen);
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
// Scientific-notation number parsing (Phase 8e — REST discovery)
// ---------------------------------------------------------------

/// Parse a JSON bare number — `[-]int[.frac][(e|E)[±]exp]` — and return
/// it scaled by `1e9` as `i64`. Returns `(value, new_pos)`.
///
/// Added in Phase 8e for Deribit REST discovery: `get_instruments`
/// emits **unquoted** floats including scientific notation
/// (`"tick_size": 1e-05`, `"contract_size": 10.0`). The WS-side
/// scanners ([`scan_price_1e6`]/[`scan_price_1e9`]) deliberately do not
/// accept exponents (never seen on the WS wire — an exponent there is
/// malformed data and must be rejected); this one does.
///
/// Integer arithmetic only. Precision below 1e-9 truncates toward zero
/// (same policy as [`scan_price_1e9`]'s digit truncation). Overflow of
/// the scaled magnitude returns `None`.
#[inline]
pub fn scan_number_sci_1e9(buf: &[u8], pos: Pos) -> Option<(i64, Pos)> {
    scan_number_sci_scaled(buf, pos, 9)
}

/// [`scan_number_sci_1e9`]'s ×1e6 sibling — the trading fixed point.
///
/// Added when the first live Deribit raw-tap (2026-08-15) showed the
/// WS `trades` channel rendering round amounts in scientific notation
/// (`"amount": 1.0e3` for a 1000-USD print) — the strict
/// [`scan_price_1e6`] rejected every such row (~1.3 % of Deribit
/// messages, and the source of its phantom trade-seq gaps). Venues
/// whose wire never uses exponents keep the strict scanner.
#[inline]
pub fn scan_number_sci_1e6(buf: &[u8], pos: Pos) -> Option<(i64, Pos)> {
    scan_number_sci_scaled(buf, pos, 6)
}

/// Shared body of the sci-notation scanners — one implementation, two
/// output scales.
#[inline]
fn scan_number_sci_scaled(buf: &[u8], pos: Pos, out_scale: i32) -> Option<(i64, Pos)> {
    if pos >= buf.len() {
        return None;
    }
    let (negative, mut i) = if buf[pos] == b'-' {
        (true, pos + 1)
    } else {
        (false, pos)
    };

    // Mantissa accumulation over int + frac digits; `dec_exp` tracks the
    // decimal-point shift (one per fractional digit).
    let mut mantissa: u128 = 0;
    let mut dec_exp: i32 = 0;
    let mut int_digits = 0usize;
    while i < buf.len() && buf[i].is_ascii_digit() {
        mantissa = mantissa
            .checked_mul(10)?
            .checked_add((buf[i] - b'0') as u128)?;
        int_digits += 1;
        i += 1;
    }
    if int_digits == 0 {
        return None;
    }
    if i < buf.len() && buf[i] == b'.' {
        i += 1;
        let mut frac_digits = 0usize;
        while i < buf.len() && buf[i].is_ascii_digit() {
            mantissa = mantissa
                .checked_mul(10)?
                .checked_add((buf[i] - b'0') as u128)?;
            dec_exp -= 1;
            frac_digits += 1;
            i += 1;
        }
        if frac_digits == 0 {
            return None; // "1." is not a JSON number
        }
    }
    let mut exp_val: i32 = 0;
    if i < buf.len() && (buf[i] == b'e' || buf[i] == b'E') {
        i += 1;
        let mut exp_neg = false;
        if i < buf.len() && (buf[i] == b'+' || buf[i] == b'-') {
            exp_neg = buf[i] == b'-';
            i += 1;
        }
        let mut exp_digits = 0usize;
        while i < buf.len() && buf[i].is_ascii_digit() {
            exp_val = exp_val.saturating_mul(10).saturating_add((buf[i] - b'0') as i32);
            exp_digits += 1;
            i += 1;
        }
        if exp_digits == 0 {
            return None; // "1e" / "1e-" are not JSON numbers
        }
        if exp_neg {
            exp_val = -exp_val;
        }
    }

    // Apply 10^(dec_exp + exp_val) and the requested output scale.
    let total_exp = dec_exp.saturating_add(exp_val).saturating_add(out_scale);
    let scaled: u128 = if total_exp >= 0 {
        if total_exp > 38 {
            return None; // 10^39 overflows u128 regardless of mantissa
        }
        mantissa.checked_mul(10u128.checked_pow(total_exp as u32)?)?
    } else {
        let down = -total_exp;
        if down > 38 {
            0
        } else {
            mantissa / 10u128.pow(down as u32)
        }
    };
    let mag = i64::try_from(scaled).ok()?;
    Some((if negative { -mag } else { mag }, i))
}

// ---------------------------------------------------------------
// JSON value skipping (Phase 8e — REST discovery object walkers)
// ---------------------------------------------------------------

/// Maximum nesting depth [`skip_json_value`] will traverse. Venue REST
/// discovery payloads nest ≤ 4 deep; anything past this cap is treated
/// as malformed.
pub const SKIP_VALUE_MAX_DEPTH: usize = 16;

/// Skip ASCII JSON whitespace. Returns the first non-whitespace
/// position (or `buf.len()`).
#[inline]
pub fn skip_ws(buf: &[u8], pos: Pos) -> Pos {
    let mut i = pos;
    while i < buf.len() && matches!(buf[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Skip a JSON string body. `pos` must point at the first byte **after**
/// the opening `"`. Returns the position after the closing quote.
/// Handles `\"` / `\\` (and all other backslash escapes as two-byte
/// units; `\uXXXX` is four ordinary bytes after the escape pair — no
/// unescaping is performed, this is a *skipper*).
#[inline]
pub fn skip_string(buf: &[u8], pos: Pos) -> Option<Pos> {
    let mut i = pos;
    while i < buf.len() {
        match buf[i] {
            b'"' => return Some(i + 1),
            b'\\' => i += 2, // escape pair; may step past end → loop exits
            _ => i += 1,
        }
    }
    None
}

/// Skip one complete JSON value starting at `pos` (leading whitespace
/// tolerated): string, number, object, array, `true`, `false`, `null`.
/// Returns the position after the value. `None` on malformed input or
/// nesting deeper than [`SKIP_VALUE_MAX_DEPTH`].
///
/// Boot-path helper for the Phase-8e REST discovery parsers (venue
/// instrument objects carry fields of every JSON type — quoted strings
/// with escapes, bare numbers, booleans, nested arrays). Iterative —
/// no recursion; the depth "stack" is only a counter because a skipper
/// never needs to know *which* container it is inside, just how many
/// remain open.
pub fn skip_json_value(buf: &[u8], pos: Pos) -> Option<Pos> {
    let mut i = skip_ws(buf, pos);
    let mut depth = 0usize;
    loop {
        if i >= buf.len() {
            return None;
        }
        match buf[i] {
            b'{' | b'[' => {
                depth += 1;
                if depth > SKIP_VALUE_MAX_DEPTH {
                    return None;
                }
                i += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'"' => {
                i = skip_string(buf, i + 1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            b',' | b':' => {
                if depth == 0 {
                    return None;
                }
                i += 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b't' => {
                i = skip_keyword(buf, i, b"true")?;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'f' => {
                i = skip_keyword(buf, i, b"false")?;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'n' => {
                i = skip_keyword(buf, i, b"null")?;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'-' | b'0'..=b'9' => {
                i = skip_number(buf, i)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => return None,
        }
    }
}

/// Skip an exact keyword (`true` / `false` / `null`).
#[inline]
fn skip_keyword(buf: &[u8], pos: Pos, kw: &[u8]) -> Option<Pos> {
    let end = pos.checked_add(kw.len())?;
    if end <= buf.len() && &buf[pos..end] == kw {
        Some(end)
    } else {
        None
    }
}

/// Skip a JSON number (integer / fraction / exponent forms).
#[inline]
fn skip_number(buf: &[u8], pos: Pos) -> Option<Pos> {
    let mut i = pos;
    if i < buf.len() && buf[i] == b'-' {
        i += 1;
    }
    let d0 = i;
    while i < buf.len() && buf[i].is_ascii_digit() {
        i += 1;
    }
    if i == d0 {
        return None;
    }
    if i < buf.len() && buf[i] == b'.' {
        i += 1;
        let f0 = i;
        while i < buf.len() && buf[i].is_ascii_digit() {
            i += 1;
        }
        if i == f0 {
            return None;
        }
    }
    if i < buf.len() && (buf[i] == b'e' || buf[i] == b'E') {
        i += 1;
        if i < buf.len() && (buf[i] == b'+' || buf[i] == b'-') {
            i += 1;
        }
        let e0 = i;
        while i < buf.len() && buf[i].is_ascii_digit() {
            i += 1;
        }
        if i == e0 {
            return None;
        }
    }
    Some(i)
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
    fn scan_price_1e9_keeps_rate_precision() {
        // OKX funding rate — the motivating case: 0.0000593 must not
        // collapse to 59 counts the way it does at 1e6 scale.
        let (v, p) = scan_price_1e9(b"0.0000593,", 0).unwrap();
        assert_eq!(v, 59_300);
        assert_eq!(p, 9);
    }

    #[test]
    fn scan_price_1e9_pads_and_truncates() {
        assert_eq!(scan_price_1e9(b"1.5", 0).unwrap().0, 1_500_000_000);
        // 12 fractional digits; keep 9.
        assert_eq!(
            scan_price_1e9(b"0.123456789123", 0).unwrap().0,
            123_456_789
        );
    }

    #[test]
    fn scan_price_1e9_handles_negative_and_rejects_garbage() {
        assert_eq!(scan_price_1e9(b"-0.000001", 0).unwrap().0, -1_000);
        assert_eq!(scan_price_1e9(b"", 0), None);
        assert_eq!(scan_price_1e9(b".", 0), None);
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

#[cfg(test)]
mod sci_and_skip_tests {
    use super::*;

    // ---- scan_number_sci_1e9: happy paths -------------------------

    #[test]
    fn sci_parses_plain_integers_and_floats() {
        assert_eq!(scan_number_sci_1e9(b"0.5,", 0), Some((500_000_000, 3)));
        assert_eq!(scan_number_sci_1e9(b"10.0}", 0), Some((10_000_000_000, 4)));
        assert_eq!(scan_number_sci_1e9(b"1,", 0), Some((1_000_000_000, 1)));
        assert_eq!(scan_number_sci_1e9(b"-2.5 ", 0), Some((-2_500_000_000, 4)));
    }

    #[test]
    fn sci_parses_deribit_wire_shapes() {
        // Live-probed 2026-08-14: BTC-PERPETUAL tick_size 0.5, USDC
        // instruments 1e-05, commissions 0.00025, contract_size 10.0.
        assert_eq!(scan_number_sci_1e9(b"1e-05,", 0), Some((10_000, 5)));
        assert_eq!(scan_number_sci_1e9(b"0.00025}", 0), Some((250_000, 7)));
        assert_eq!(scan_number_sci_1e9(b"1E-05", 0), Some((10_000, 5)));
        assert_eq!(scan_number_sci_1e9(b"2.5e2", 0), Some((250_000_000_000, 5)));
        assert_eq!(scan_number_sci_1e9(b"5e+1", 0), Some((50_000_000_000, 4)));
    }

    #[test]
    fn sci_1e6_parses_the_live_deribit_reject_shapes() {
        // Real wire values from the 2026-08-15 raw tap: round trade
        // amounts render as scientific notation.
        assert_eq!(scan_number_sci_1e6(b"1.0e3,", 0), Some((1_000_000_000, 5)));
        assert_eq!(scan_number_sci_1e6(b"209.0,", 0), Some((209_000_000, 5)));
        assert_eq!(scan_number_sci_1e6(b"62863.68,", 0), Some((62_863_680_000, 8)));
        assert_eq!(scan_number_sci_1e6(b"2.5e-2}", 0), Some((25_000, 6)));
        assert_eq!(scan_number_sci_1e6(b"1e30", 0), None); // overflow
        assert_eq!(scan_number_sci_1e6(b"e3", 0), None);
    }

    #[test]
    fn sci_truncates_below_1e9_resolution() {
        // 1e-12 < 1e-9 resolution → truncates toward zero.
        assert_eq!(scan_number_sci_1e9(b"1e-12", 0), Some((0, 5)));
        assert_eq!(scan_number_sci_1e9(b"-1e-12", 0), Some((0, 6)));
    }

    // ---- scan_number_sci_1e9: failure modes -----------------------

    #[test]
    fn sci_rejects_malformed() {
        assert_eq!(scan_number_sci_1e9(b"", 0), None);
        assert_eq!(scan_number_sci_1e9(b"-", 0), None);
        assert_eq!(scan_number_sci_1e9(b".5", 0), None);
        assert_eq!(scan_number_sci_1e9(b"1.", 0), None);
        assert_eq!(scan_number_sci_1e9(b"1e", 0), None);
        assert_eq!(scan_number_sci_1e9(b"1e-", 0), None);
        assert_eq!(scan_number_sci_1e9(b"abc", 0), None);
    }

    #[test]
    fn sci_rejects_overflow() {
        // 1e30 × 1e9 scale overflows i64.
        assert_eq!(scan_number_sci_1e9(b"1e30", 0), None);
        assert_eq!(scan_number_sci_1e9(b"99999999999999999999e9", 0), None);
    }

    // ---- skip_string ----------------------------------------------

    #[test]
    fn skip_string_handles_escapes() {
        // pos is after the opening quote.
        assert_eq!(skip_string(b"abc\"X", 0), Some(4));
        assert_eq!(skip_string(b"a\\\"b\"X", 0), Some(5));
        assert_eq!(skip_string(b"a\\\\\"X", 0), Some(4));
        assert_eq!(skip_string(b"never terminated", 0), None);
        // Trailing lone backslash must not panic (escape steps past end).
        assert_eq!(skip_string(b"abc\\", 0), None);
    }

    // ---- skip_json_value: happy paths -----------------------------

    #[test]
    fn skip_value_covers_all_scalar_kinds() {
        assert_eq!(skip_json_value(b"\"str\",", 0), Some(5));
        assert_eq!(skip_json_value(b"-12.5e3,", 0), Some(7));
        assert_eq!(skip_json_value(b"true,", 0), Some(4));
        assert_eq!(skip_json_value(b"false]", 0), Some(5));
        assert_eq!(skip_json_value(b"null}", 0), Some(4));
        assert_eq!(skip_json_value(b"  42", 0), Some(4));
    }

    #[test]
    fn skip_value_covers_nested_containers() {
        let v = br#"{"a":[1,2,{"b":"x\"y"}],"c":false},NEXT"#;
        let end = skip_json_value(v, 0).unwrap();
        assert_eq!(&v[end..end + 5], b",NEXT");
        let a = br#"[[],[{"k":[true,null]}]] "#;
        assert_eq!(skip_json_value(a, 0), Some(24));
    }

    #[test]
    fn skip_value_real_okx_instrument_object() {
        // Trimmed live-probe row: quoted strings, bare number
        // (instIdCode), bare bool, nested array.
        let v = br#"{"instId":"BTC-USDT-SWAP","instIdCode":10459,"futureSettlement":false,"tradeQuoteCcyList":[],"tickSz":"0.1"},{"#;
        let end = skip_json_value(v, 0).unwrap();
        assert_eq!(v[end], b',');
    }

    // ---- skip_json_value: failure modes ---------------------------

    #[test]
    fn skip_value_rejects_malformed() {
        assert_eq!(skip_json_value(b"", 0), None);
        assert_eq!(skip_json_value(b"   ", 0), None);
        assert_eq!(skip_json_value(b"{", 0), None);
        assert_eq!(skip_json_value(b"}", 0), None);
        assert_eq!(skip_json_value(b"[1,2", 0), None);
        assert_eq!(skip_json_value(b"\"open", 0), None);
        assert_eq!(skip_json_value(b"tru", 0), None);
        assert_eq!(skip_json_value(b"@", 0), None);
        assert_eq!(skip_json_value(b",", 0), None);
    }

    #[test]
    fn skip_value_rejects_depth_bomb() {
        let mut v = [0u8; SKIP_VALUE_MAX_DEPTH + 2];
        for b in v.iter_mut() {
            *b = b'[';
        }
        assert_eq!(skip_json_value(&v, 0), None);
    }
}

#[cfg(test)]
mod sci_and_skip_proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Panic-free on arbitrary input; returned positions in-bounds
        /// and strictly advancing.
        #[test]
        fn sci_scan_in_bounds(input in proptest::collection::vec(any::<u8>(), 0..256), pos in 0usize..300) {
            if let Some((_, end)) = scan_number_sci_1e9(&input, pos) {
                prop_assert!(end <= input.len());
                prop_assert!(end > pos);
            }
        }

        /// skip_json_value never panics; on success the end is in
        /// bounds and past the start.
        #[test]
        fn skip_value_in_bounds(input in proptest::collection::vec(any::<u8>(), 0..512), pos in 0usize..600) {
            if let Some(end) = skip_json_value(&input, pos) {
                prop_assert!(end <= input.len());
                prop_assert!(end > pos);
            }
        }

        /// Agreement: any number scan_price_1e9 accepts (no exponent),
        /// scan_number_sci_1e9 accepts with the identical value.
        #[test]
        fn sci_agrees_with_plain_1e9(int_part in 0u64..1_000_000, frac in 0u32..1_000_000_000u32) {
            let s = format!("{int_part}.{frac:09}");
            let b = s.as_bytes();
            let plain = scan_price_1e9(b, 0);
            let sci = scan_number_sci_1e9(b, 0);
            prop_assert!(plain.is_some());
            prop_assert_eq!(plain, sci);
        }
    }
}
