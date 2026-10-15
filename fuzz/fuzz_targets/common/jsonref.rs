// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **An independent JSON walk for the fuzz targets' oracles** (moved out of
//! `bn_spot_event`, BX13: `bn_wsapi_frame` judges spot's day count with it
//! too). Loose where the scanners are loose — the bounds of a value — and
//! naming each object's members in order, so an oracle can ask how many
//! times a key is named. Allocates freely (an oracle, never the code under
//! test).

// Each target includes this module whole and reads what it needs of it.
#![allow(dead_code)]

/// An object's top-level pairs, in order: (key, raw value), slices of the
/// frame.
pub type Members<'a> = Vec<(&'a [u8], &'a [u8])>;

/// JSON's whitespace from `i`.
pub fn ws(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// Past the string whose opening quote is at `i` (a backslash takes the
/// byte after it).
pub fn past_string(b: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    loop {
        match b.get(i)? {
            b'"' => return Some(i + 1),
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
}

/// Past the value at `i`, read loosely — a string; a container to its
/// matching bracket, the strings inside it skipped; a scalar to the next
/// delimiter: the bounds the scanners see in any frame they accept.
pub fn past_value(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i)? {
        b'"' => past_string(b, i),
        b'{' | b'[' => {
            let (mut depth, mut j) = (0usize, i);
            loop {
                match b.get(j)? {
                    b'"' => {
                        j = past_string(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        _ => {
            let mut j = i;
            while !matches!(b.get(j), None | Some(b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')) {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// The top-level pairs of the object at `at` (whitespace before it
/// allowed), or `None` if no object starts there.
pub fn pairs(b: &[u8], at: usize) -> Option<Members<'_>> {
    let mut i = ws(b, at);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    let mut out = Vec::new();
    i = ws(b, i + 1);
    if b.get(i) == Some(&b'}') {
        return Some(out);
    }
    loop {
        if b.get(i) != Some(&b'"') {
            return None;
        }
        let k1 = past_string(b, i)?;
        let key = b.get(i + 1..k1 - 1)?;
        i = ws(b, k1);
        if b.get(i) != Some(&b':') {
            return None;
        }
        let v0 = ws(b, i + 1);
        let v1 = past_value(b, v0)?;
        out.push((key, b.get(v0..v1)?));
        i = ws(b, v1);
        match b.get(i)? {
            b',' => i = ws(b, i + 1),
            b'}' => return Some(out),
            _ => return None,
        }
    }
}

/// The elements of the array at `at`, or `None` if no array starts there.
pub fn elems(b: &[u8], at: usize) -> Option<Vec<&[u8]>> {
    let mut i = ws(b, at);
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let mut out = Vec::new();
    i = ws(b, i + 1);
    if b.get(i) == Some(&b']') {
        return Some(out);
    }
    loop {
        let e = past_value(b, i)?;
        out.push(b.get(i..e)?);
        i = ws(b, e);
        match b.get(i)? {
            b',' => i = ws(b, i + 1),
            b']' => return Some(out),
            _ => return None,
        }
    }
}

/// How many times the object names `key`.
pub fn count(p: &[(&[u8], &[u8])], key: &str) -> usize {
    p.iter().filter(|(k, _)| *k == key.as_bytes()).count()
}

/// The raw value of `key` when the object names it exactly once.
pub fn one<'a>(p: &[(&'a [u8], &'a [u8])], key: &str) -> Option<&'a [u8]> {
    let mut named = p.iter().filter(|(k, _)| *k == key.as_bytes());
    let v = named.next()?.1;
    named.next().is_none().then_some(v)
}

/// A string value's raw bytes (nothing unescaped), or `None`.
pub fn text(v: &[u8]) -> Option<&[u8]> {
    (v.len() >= 2 && v.first() == Some(&b'"') && v.last() == Some(&b'"')).then(|| &v[1..v.len() - 1])
}

/// A number's bytes: a string's, or the bare value's.
pub fn num(v: &[u8]) -> Option<&[u8]> {
    if v.first() == Some(&b'"') {
        text(v)
    } else {
        Some(v)
    }
}

/// An id: 1 to 19 digits.
pub fn id(v: &[u8]) -> Option<u64> {
    let n = num(v)?;
    if n.is_empty() || n.len() > 19 || !n.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(n.iter().fold(0, |v, c| v * 10 + u64::from(c - b'0')))
}
