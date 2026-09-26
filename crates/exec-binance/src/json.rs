// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The one JSON walker the scanners share.** In place, one pass, no
//! allocation, no recursion, never a panic.
//!
//! [`Pairs`] walks the `"key": value` pairs of ONE object at its own
//! depth — a nested object or array is skipped whole (so a key inside it
//! can never be mistaken for one of the outer object's), and the caller
//! dispatches on the key bytes. [`Elems`] walks the elements of one
//! array. Values are handed out as byte spans of the frame; the typed
//! readers ([`str_of`], [`dec_1e6`], [`u64_of`], [`i64_of`], [`bool_of`])
//! accept Binance's habit of quoting numbers.
//!
//! Every malformed input ends the walk with `Err(())`; the scanners turn
//! that into their own refusal. No `unwrap`, no index past a bound.

use core_parse::{skip_json_value, skip_string, skip_ws};

/// The pairs of one JSON object.
pub struct Pairs<'a> {
    b: &'a [u8],
    pos: usize,
    first: bool,
    done: bool,
}

impl<'a> Pairs<'a> {
    /// Walk the object whose `{` is at or after `pos` (whitespace
    /// allowed). `Err` if no object starts there.
    pub fn new(b: &'a [u8], pos: usize) -> Result<Self, ()> {
        let p = skip_ws(b, pos);
        if b.get(p) != Some(&b'{') {
            return Err(());
        }
        Ok(Self {
            b,
            pos: p + 1,
            first: true,
            done: false,
        })
    }

    /// The next pair as (key bytes, value span start, value span end), or
    /// `Ok(None)` at the closing brace.
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<(&'a [u8], usize, usize)>, ()> {
        if self.done {
            return Ok(None);
        }
        let b = self.b;
        let mut p = skip_ws(b, self.pos);
        if b.get(p) == Some(&b'}') {
            self.done = true;
            self.pos = p + 1;
            return Ok(None);
        }
        if !self.first {
            if b.get(p) != Some(&b',') {
                return Err(());
            }
            p = skip_ws(b, p + 1);
        }
        self.first = false;
        if b.get(p) != Some(&b'"') {
            return Err(());
        }
        let k0 = p + 1;
        let k1 = skip_string(b, k0).ok_or(())?;
        let key = b.get(k0..k1 - 1).ok_or(())?;
        let c = skip_ws(b, k1);
        if b.get(c) != Some(&b':') {
            return Err(());
        }
        let v0 = skip_ws(b, c + 1);
        let v1 = skip_json_value(b, v0).ok_or(())?;
        self.pos = v1;
        Ok(Some((key, v0, v1)))
    }
}

/// The elements of one JSON array.
pub struct Elems<'a> {
    b: &'a [u8],
    pos: usize,
    first: bool,
    done: bool,
}

impl<'a> Elems<'a> {
    /// Walk the array whose `[` is at or after `pos`.
    pub fn new(b: &'a [u8], pos: usize) -> Result<Self, ()> {
        let p = skip_ws(b, pos);
        if b.get(p) != Some(&b'[') {
            return Err(());
        }
        Ok(Self {
            b,
            pos: p + 1,
            first: true,
            done: false,
        })
    }

    /// The next element's span, or `Ok(None)` at the closing bracket.
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<(usize, usize)>, ()> {
        if self.done {
            return Ok(None);
        }
        let b = self.b;
        let mut p = skip_ws(b, self.pos);
        if b.get(p) == Some(&b']') {
            self.done = true;
            self.pos = p + 1;
            return Ok(None);
        }
        if !self.first {
            if b.get(p) != Some(&b',') {
                return Err(());
            }
            p = skip_ws(b, p + 1);
        }
        self.first = false;
        let e = skip_json_value(b, p).ok_or(())?;
        self.pos = e;
        Ok(Some((p, e)))
    }
}

/// A string value's bytes (without the quotes; escapes left as they are —
/// every field read this way is ASCII without escapes, and a backslash
/// makes it a mismatch, never a panic).
#[inline]
pub fn str_of(b: &[u8], v0: usize, v1: usize) -> Result<&[u8], ()> {
    if v1 < v0 + 2 || b.get(v0) != Some(&b'"') || b.get(v1 - 1) != Some(&b'"') {
        return Err(());
    }
    b.get(v0 + 1..v1 - 1).ok_or(())
}

/// The bytes of a number, quoted or bare.
#[inline]
fn num_bytes(b: &[u8], v0: usize, v1: usize) -> Result<&[u8], ()> {
    if b.get(v0) == Some(&b'"') {
        str_of(b, v0, v1)
    } else {
        b.get(v0..v1).ok_or(())
    }
}

/// A decimal (quoted or bare, optional `-`, ≤ 6 decimals kept) ×1e6. The
/// whole value must be the number: a trailing byte is a refusal. The
/// integer part has at most 18 digits — the integer scanner wraps past 19
/// without a signal, and a wrapped numeral can read as any small value
/// (a plausible quantity made from nonsense) — so a longer one is refused,
/// never defaulted; the scale's own overflow is refused by the scanner.
#[inline]
pub fn dec_1e6(b: &[u8], v0: usize, v1: usize) -> Result<i64, ()> {
    let n = num_bytes(b, v0, v1)?;
    let mut i = (n.first() == Some(&b'-')) as usize;
    let int0 = i;
    while i < n.len() && n[i].is_ascii_digit() {
        i += 1;
    }
    if i - int0 > 18 {
        return Err(());
    }
    let (v, end) = core_parse::scan_price_1e6(n, 0).ok_or(())?;
    if end != n.len() {
        return Err(());
    }
    Ok(v)
}

/// An unsigned integer (quoted or bare), at most 19 digits (so it cannot
/// wrap).
#[inline]
pub fn u64_of(b: &[u8], v0: usize, v1: usize) -> Result<u64, ()> {
    let n = num_bytes(b, v0, v1)?;
    if n.is_empty() || n.len() > 19 {
        return Err(());
    }
    let (v, end) = core_parse::scan_u64(n, 0).ok_or(())?;
    if end != n.len() {
        return Err(());
    }
    Ok(v)
}

/// A signed integer (quoted or bare), at most 18 digits after the sign.
#[inline]
pub fn i64_of(b: &[u8], v0: usize, v1: usize) -> Result<i64, ()> {
    let n = num_bytes(b, v0, v1)?;
    let (neg, d) = match n.first() {
        Some(b'-') => (true, n.get(1..).ok_or(())?),
        _ => (false, n),
    };
    if d.is_empty() || d.len() > 18 {
        return Err(());
    }
    let (v, end) = core_parse::scan_u64(d, 0).ok_or(())?;
    if end != d.len() {
        return Err(());
    }
    let v = v as i64;
    Ok(if neg { -v } else { v })
}

/// A JSON boolean.
#[inline]
pub fn bool_of(b: &[u8], v0: usize, v1: usize) -> Result<bool, ()> {
    match b.get(v0..v1) {
        Some(b"true") => Ok(true),
        Some(b"false") => Ok(false),
        _ => Err(()),
    }
}

/// `null`.
#[inline]
#[must_use]
pub fn is_null(b: &[u8], v0: usize, v1: usize) -> bool {
    b.get(v0..v1) == Some(b"null")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn walks_one_level_and_skips_nesting() {
        let f = br#" {"e":"X","o":{"e":"inner","s":"BTC"},"a":[1,{"x":2}],"n":"12.5","b":true} "#;
        let mut w = Pairs::new(f, 0).unwrap();
        let mut keys = std::vec::Vec::new();
        while let Some((k, v0, v1)) = w.next().unwrap() {
            keys.push(k.to_vec());
            if k == b"n" {
                assert_eq!(dec_1e6(f, v0, v1), Ok(12_500_000));
            }
            if k == b"b" {
                assert_eq!(bool_of(f, v0, v1), Ok(true));
            }
            if k == b"o" {
                let mut inner = Pairs::new(f, v0).unwrap();
                let (k2, a, z) = inner.next().unwrap().unwrap();
                assert_eq!((k2, str_of(f, a, z).unwrap()), (&b"e"[..], &b"inner"[..]));
            }
        }
        assert_eq!(keys, [b"e".to_vec(), b"o".to_vec(), b"a".to_vec(), b"n".to_vec(), b"b".to_vec()]);
        let mut bad = Pairs::new(br#"{"a":1"b":2}"#, 0).unwrap();
        assert!(bad.next().is_ok());
        assert!(bad.next().is_err(), "a missing comma is refused");
    }

    #[test]
    fn arrays() {
        let f = br#"[{"a":1}, {"a":2} ,3]"#;
        let mut e = Elems::new(f, 0).unwrap();
        let mut n = 0;
        while let Some((a, z)) = e.next().unwrap() {
            n += 1;
            assert!(z > a);
        }
        assert_eq!(n, 3);
        assert!(Elems::new(b"[1 2]", 0).and_then(|mut e| { e.next()?; e.next() }).is_err());
        let mut empty = Elems::new(b"[ ]", 0).unwrap();
        assert_eq!(empty.next(), Ok(None));
    }

    #[test]
    fn numbers_are_whole_or_refused() {
        let f = br#""7" 12 "-3" "0.001" "1e5" 99999999999999999999 "1x""#;
        assert_eq!(u64_of(f, 0, 3), Ok(7));
        assert_eq!(u64_of(f, 4, 6), Ok(12));
        assert_eq!(i64_of(f, 7, 11), Ok(-3));
        assert_eq!(dec_1e6(f, 12, 19), Ok(1_000));
        assert!(dec_1e6(f, 20, 25).is_err(), "no exponent");
        assert!(u64_of(f, 26, 46).is_err(), "20 digits could wrap");
        assert!(u64_of(f, 47, 51).is_err());
    }

    proptest! {
        #[test]
        fn never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256), at in 0usize..300) {
            if let Ok(mut w) = Pairs::new(&bytes, at) {
                for _ in 0..64 {
                    match w.next() {
                        Ok(Some((_, a, z))) => {
                            let _ = (str_of(&bytes, a, z), dec_1e6(&bytes, a, z), u64_of(&bytes, a, z), i64_of(&bytes, a, z), bool_of(&bytes, a, z));
                        }
                        _ => break,
                    }
                }
            }
            if let Ok(mut e) = Elems::new(&bytes, at) {
                for _ in 0..64 {
                    if !matches!(e.next(), Ok(Some(_))) {
                        break;
                    }
                }
            }
        }
    }
}
