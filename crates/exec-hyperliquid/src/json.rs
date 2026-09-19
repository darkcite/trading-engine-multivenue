// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Small JSON field readers shared by the two scanners that decide what
//! the engine believes it owns — `userws` (the fills) and `recon` (the
//! balances). ONE copy: the E7 review found the two files each
//! carrying a byte-identical `object_end` / `string_field` /
//! `decimal_field`, which is two places for one parsing rule to
//! drift on exactly the two paths where a drift books a position
//! against the wrong name.
//!
//! Ingress house style: byte scanners over `&[u8]`, spans into the
//! caller's buffer, no allocation, no `serde_json`.

use core_parse::{find_field, scan_price_1e8, scan_u64, skip_ws};

use crate::response::Span;

/// Byte just past `"key":` with whitespace skipped, or `None`.
#[inline]
fn value_start(b: &[u8], key: &[u8]) -> Option<usize> {
    let p = find_field(b, key)?;
    let i = skip_ws(b, p);
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    Some(skip_ws(b, i + 1))
}

/// Byte just past `"key":[`, or `None`.
#[inline]
pub(crate) fn array_start(b: &[u8], key: &[u8]) -> Option<usize> {
    let i = value_start(b, key)?;
    if i >= b.len() || b[i] != b'[' {
        return None;
    }
    Some(i + 1)
}

/// Find the byte just past the object starting at `start`.
pub(crate) fn object_end(b: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = start;
    let mut in_str = false;
    let mut esc = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// `"key":"value"` → the span of `value`.
pub(crate) fn string_field(b: &[u8], key: &[u8]) -> Option<Span> {
    let mut i = value_start(b, key)?;
    if i >= b.len() || b[i] != b'"' {
        return None;
    }
    i += 1;
    let start = i;
    while i < b.len() && b[i] != b'"' {
        if b[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    Some(Span {
        start: start as u32,
        end: i as u32,
    })
}

/// `"key":"12.34"` → `1_234_000_000`. The venue quotes numbers as
/// STRINGS; a bare number is accepted too rather than refused.
pub(crate) fn decimal_field(b: &[u8], key: &[u8]) -> Option<i64> {
    let mut i = value_start(b, key)?;
    if i < b.len() && b[i] == b'"' {
        i += 1;
    }
    scan_price_1e8(b, i).map(|(v, _)| v)
}

/// `"key":123` or `"key":"123"` → `123`.
pub(crate) fn u64_field(b: &[u8], key: &[u8]) -> Option<u64> {
    let mut i = value_start(b, key)?;
    if i < b.len() && b[i] == b'"' {
        i += 1;
    }
    scan_u64(b, i).map(|(v, _)| v)
}

/// `"key":true|false`.
pub(crate) fn bool_field(b: &[u8], key: &[u8]) -> Option<bool> {
    let i = value_start(b, key)?;
    if b[i..].starts_with(b"true") {
        Some(true)
    } else if b[i..].starts_with(b"false") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &[u8] = br#"{"a":"x\"y","n":"12.5","m":7,"t":true,"f":false,"arr":[{"k":1},{"k":2}]}"#;

    #[test]
    fn every_reader_finds_its_field() {
        let s = string_field(DOC, b"\"a\"").unwrap();
        assert_eq!(s.of(DOC), br#"x\"y"#);
        assert_eq!(decimal_field(DOC, b"\"n\""), Some(1_250_000_000));
        assert_eq!(u64_field(DOC, b"\"m\""), Some(7));
        assert_eq!(bool_field(DOC, b"\"t\""), Some(true));
        assert_eq!(bool_field(DOC, b"\"f\""), Some(false));
        let a = array_start(DOC, b"\"arr\"").unwrap();
        assert_eq!(DOC[a], b'{');
        let e = object_end(DOC, a).unwrap();
        assert_eq!(&DOC[a..e], br#"{"k":1}"#);
    }

    #[test]
    fn a_missing_or_malformed_field_is_none_never_a_panic() {
        assert!(string_field(DOC, b"\"zz\"").is_none());
        assert!(string_field(br#"{"a":"unterminated"#, b"\"a\"").is_none());
        assert!(decimal_field(br#"{"n":}"#, b"\"n\"").is_none());
        assert!(object_end(b"{\"a\":{", 0).is_none());
        assert!(array_start(br#"{"arr":{}}"#, b"\"arr\"").is_none());
    }
}
