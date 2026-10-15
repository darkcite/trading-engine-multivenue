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
//! readers ([`str_of`], [`dec_1e6`], [`dec_1e9`], [`u64_of`], [`i64_of`], [`bool_of`])
//! accept Binance's habit of quoting numbers.
//!
//! Every malformed input ends the walk with `Err(())`; the scanners turn
//! that into their own refusal. No `unwrap`, no index past a bound.
//!
//! **A key named twice** (BX7): the walker hands out every pair, so a
//! reader decides. The readers BX7 added or changed refuse a key they read
//! that an object names twice — no second reader of the frame can then
//! pick another copy than the first did ([`field`] for one top-level key;
//! the scanners keep a bit per key they read).
//!
//! **Public since BX7b** for the cli's Demo tools (`cli::bn_demo`), so
//! their cold reads walk JSON the way the scanners do rather than by a
//! second walker. The errors carry nothing by design: each caller turns
//! `()` into its own refusal.

#![allow(clippy::result_unit_err)]

use core_parse::{skip_json_value, skip_ws};

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
        // BX7b: a key with an escape is refused. Keys are compared raw, so
        // `"bid\u0050rice"` would read as another key than `bidPrice` and
        // slip past the one-reading law; Binance names no key with one.
        // The key is read once, to its closing quote (the alloc review,
        // round 2: no second walk over it).
        let mut k1 = k0;
        loop {
            match b.get(k1) {
                Some(b'"') => break,
                Some(b'\\') | None => return Err(()),
                Some(_) => k1 += 1,
            }
        }
        let key = b.get(k0..k1).ok_or(())?;
        let c = skip_ws(b, k1 + 1);
        if b.get(c) != Some(&b':') {
            return Err(());
        }
        let v0 = skip_ws(b, c + 1);
        let v1 = skip_json_value(b, v0).ok_or(())?;
        self.pos = v1;
        Ok(Some((key, v0, v1)))
    }

    /// Where the walk ended — just past the closing brace — once
    /// [`Pairs::next`] has answered `Ok(None)`; `None` before. A reader that
    /// must see the body whole checks what follows (one pass, no pre-walk).
    #[inline]
    #[must_use]
    pub const fn end(&self) -> Option<usize> {
        if self.done {
            Some(self.pos)
        } else {
            None
        }
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

    /// Where the walk ended — just past the closing bracket — once
    /// [`Elems::next`] has answered `Ok(None)`; `None` before.
    #[inline]
    #[must_use]
    pub const fn end(&self) -> Option<usize> {
        if self.done {
            Some(self.pos)
        } else {
            None
        }
    }
}

/// **The value span of `key` in the object at the start of `b`** — a key of
/// that object itself, never a nested one — or `None`: absent, named twice
/// (refused, never chosen between: BX7), or `b` no well-formed object.
/// Cold (the boot's judges): it walks the whole object.
pub fn field(b: &[u8], key: &[u8]) -> Option<(usize, usize)> {
    let mut w = Pairs::new(b, 0).ok()?;
    let mut v = None;
    while let Some((k, a, z)) = w.next().ok()? {
        if k == key {
            if v.is_some() {
                return None;
            }
            v = Some((a, z));
        }
    }
    v
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
/// integer part has at most 18 digits — a longer one is refused, never
/// defaulted; the scale's own overflow is refused by the scanner.
#[inline]
pub fn dec_1e6(b: &[u8], v0: usize, v1: usize) -> Result<i64, ()> {
    dec_scaled::<6>(b, v0, v1)
}

/// [`dec_1e6`]'s law at ×1e8 (≤ 8 decimals kept): a spot balance (BX8),
/// at the venue's own precision — a base-asset commission is finer than a
/// millionth.
#[inline]
pub fn dec_1e8(b: &[u8], v0: usize, v1: usize) -> Result<i64, ()> {
    dec_scaled::<8>(b, v0, v1)
}

/// [`dec_1e6`]'s law at ×1e9 (≤ 9 decimals kept), for an amount finer
/// than a millionth (BX11: a commission — COIN-M's is charged in the coin,
/// 8 decimals, where ×1e6 keeps nothing of a small trade's).
#[inline]
pub fn dec_1e9(b: &[u8], v0: usize, v1: usize) -> Result<i64, ()> {
    dec_scaled::<9>(b, v0, v1)
}

/// The scales' one reader: ×10^`DIGITS` (6, 8 or 9).
#[inline(always)]
fn dec_scaled<const DIGITS: u8>(b: &[u8], v0: usize, v1: usize) -> Result<i64, ()> {
    let n = num_bytes(b, v0, v1)?;
    let mut i = (n.first() == Some(&b'-')) as usize;
    let int0 = i;
    while i < n.len() && n[i].is_ascii_digit() {
        i += 1;
    }
    if i - int0 > 18 {
        return Err(());
    }
    let scanned = match DIGITS {
        9 => core_parse::scan_price_1e9(n, 0),
        8 => core_parse::scan_price_1e8(n, 0),
        _ => core_parse::scan_price_1e6(n, 0),
    };
    let (v, end) = scanned.ok_or(())?;
    if end != n.len() {
        return Err(());
    }
    Ok(v)
}

/// An unsigned integer (quoted or bare), at most 19 digits.
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

    /// BX8: ×1e8 keeps a spot balance's 8 decimals (×1e6 would drop the
    /// last two).
    #[test]
    fn eight_decimals_at_1e8() {
        let b = br#""0.00099900""#;
        assert_eq!(dec_1e8(b, 0, b.len()), Ok(99_900));
        assert_eq!(dec_1e6(b, 0, b.len()), Ok(999));
        let c = br#""0.00000001""#;
        assert_eq!((dec_1e8(c, 0, c.len()), dec_1e6(c, 0, c.len())), (Ok(1), Ok(0)));
        let w = br#""1000000.5""#;
        assert_eq!(dec_1e8(w, 0, w.len()), Ok(100_000_050_000_000));
        assert!(dec_1e8(b"x", 0, 1).is_err());
    }

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
        assert_eq!(w.end(), Some(f.len() - 1), "just past the closing brace, before the trailing space");
        let mut open = Pairs::new(br#"{"k":"unterminated}"#, 0).unwrap();
        assert!(open.next().is_err(), "a value that never closes");
        let mut cut = Pairs::new(br#"{"k"#, 0).unwrap();
        assert!(cut.next().is_err(), "a key that never closes");
        assert_eq!(cut.end(), None);
        let mut bad = Pairs::new(br#"{"a":1"b":2}"#, 0).unwrap();
        assert!(bad.next().is_ok());
        assert!(bad.next().is_err(), "a missing comma is refused");
    }

    /// BX7b: a key carrying an escape is refused, never compared raw.
    #[test]
    fn a_key_with_an_escape_is_refused() {
        let mut w = Pairs::new(br#"{"bid\u0050rice":"2"}"#, 0).unwrap();
        assert!(w.next().is_err());
        assert_eq!(field(br#"{"bidPrice":"1","bid\u0050rice":"2"}"#, b"bidPrice"), None, "never chosen between");
        let mut ok = Pairs::new(br#"{"bidPrice":"a\"b"}"#, 0).unwrap();
        assert!(ok.next().is_ok(), "an escape in a VALUE is the value's reader's business");
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
        assert_eq!(e.end(), Some(f.len()), "just past the closing bracket");
        assert!(Elems::new(b"[1 2]", 0).and_then(|mut e| { e.next()?; e.next() }).is_err());
        let mut empty = Elems::new(b"[ ] x", 0).unwrap();
        assert_eq!(empty.end(), None, "not walked yet");
        assert_eq!(empty.next(), Ok(None));
        assert_eq!(empty.end(), Some(3), "the walk ends at the bracket, not at what follows");
    }

    /// BX7: one top-level key, named once — a nested one is not it, a
    /// second copy refuses, a malformed object reads as none.
    #[test]
    fn a_field_is_the_objects_own_and_named_once() {
        let f = br#"{"x":{"k":1},"k":true}"#;
        assert_eq!(field(f, b"k").map(|(a, z)| &f[a..z]), Some(&b"true"[..]));
        assert_eq!(field(br#"{"x":{"k":1}}"#, b"k"), None, "a nested key is not the object's");
        assert_eq!(field(br#"{"k":1,"k":2}"#, b"k"), None, "named twice");
        assert_eq!(field(br#"{"k":1"#, b"k"), None, "malformed");
    }

    #[test]
    fn numbers_are_whole_or_refused() {
        let f = br#""7" 12 "-3" "0.001" "1e5" 99999999999999999999 "1x""#;
        assert_eq!(u64_of(f, 0, 3), Ok(7));
        assert_eq!(u64_of(f, 4, 6), Ok(12));
        assert_eq!(i64_of(f, 7, 11), Ok(-3));
        assert_eq!(dec_1e6(f, 12, 19), Ok(1_000));
        assert_eq!(dec_1e9(f, 12, 19), Ok(1_000_000));
        assert!(dec_1e6(f, 20, 25).is_err(), "no exponent");
        assert!(dec_1e9(f, 20, 25).is_err(), "no exponent");
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
                            let _ = (str_of(&bytes, a, z), dec_1e6(&bytes, a, z), dec_1e8(&bytes, a, z), dec_1e9(&bytes, a, z), u64_of(&bytes, a, z), i64_of(&bytes, a, z), bool_of(&bytes, a, z));
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
