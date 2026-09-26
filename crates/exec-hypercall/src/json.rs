// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! An allocation-free JSON walker over borrowed bytes — enough to read
//! the venue's answers by **top-level** key.
//!
//! Why not `core_parse::find_field`: that is a substring search, which
//! is right for the ingress's fixed frames and wrong here. An order
//! answer carries `order_id` twice (top level and inside `info`), a
//! cancel answer nests the whole order under `data`, and a `reason`
//! string can contain any key text. A walker that knows depth reads the
//! one it means.
//!
//! Every function returns `None` on anything it cannot walk; the
//! callers treat `None` as a refusal (fail closed). No recursion: a
//! container is skipped with an explicit depth counter.

use core::ops::Range;

/// What a value is.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A string; the span is INSIDE the quotes, escapes not decoded.
    Str,
    /// A number token.
    Num,
    /// An object, braces included.
    Obj,
    /// An array, brackets included.
    Arr,
    /// `true`, `false` or `null`.
    Lit,
}

/// A body that could not be walked (or a key that is ambiguous).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Malformed;

/// One value: its kind and its byte span in the walked buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Val {
    /// The kind.
    pub kind: Kind,
    /// The span (see [`Kind`]).
    pub span: Range<usize>,
}

impl Val {
    /// The value's bytes in `buf`.
    #[inline]
    #[must_use]
    pub fn bytes<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        &buf[self.span.clone()]
    }

    /// A non-negative integer value (`Num` without sign, point or
    /// exponent), else `None`.
    #[must_use]
    pub fn as_u64(&self, buf: &[u8]) -> Option<u64> {
        if self.kind != Kind::Num {
            return None;
        }
        let b = self.bytes(buf);
        if b.is_empty() {
            return None;
        }
        let mut v: u64 = 0;
        let mut i = 0usize;
        while i < b.len() {
            let c = b[i];
            if !c.is_ascii_digit() {
                return None;
            }
            v = v.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
            i += 1;
        }
        Some(v)
    }

    /// `Some(true|false)` for a `true`/`false` literal.
    #[must_use]
    pub fn as_bool(&self, buf: &[u8]) -> Option<bool> {
        match (self.kind, self.bytes(buf)) {
            (Kind::Lit, b"true") => Some(true),
            (Kind::Lit, b"false") => Some(false),
            _ => None,
        }
    }

    /// Is it `null`?
    #[must_use]
    pub fn is_null(&self, buf: &[u8]) -> bool {
        self.kind == Kind::Lit && self.bytes(buf) == b"null"
    }
}

/// First non-whitespace index at or after `i`.
#[inline]
#[must_use]
pub fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// `b[i]` is `"`: the index just past the closing quote.
fn end_of_string(b: &[u8], i: usize) -> Option<usize> {
    debug_assert!(i < b.len() && b[i] == b'"');
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'"' => return Some(j + 1),
            b'\\' => {
                // Any escaped byte; `\uXXXX` needs its four hex digits.
                let e = *b.get(j + 1)?;
                if e == b'u' {
                    let h = b.get(j + 2..j + 6)?;
                    if !h.iter().all(u8::is_ascii_hexdigit) {
                        return None;
                    }
                    j += 6;
                } else if matches!(e, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') {
                    j += 2;
                } else {
                    return None;
                }
            }
            c if c < 0x20 => return None,
            _ => j += 1,
        }
    }
    None
}

/// `b[i]` opens a container: the index just past its matching close.
fn end_of_container(b: &[u8], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'"' => {
                j = end_of_string(b, j)?;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// The value starting at `i` (after whitespace): it and the index past it.
#[must_use]
pub fn value_at(b: &[u8], i: usize) -> Option<(Val, usize)> {
    let i = skip_ws(b, i);
    let c = *b.get(i)?;
    match c {
        b'"' => {
            let e = end_of_string(b, i)?;
            Some((Val { kind: Kind::Str, span: i + 1..e - 1 }, e))
        }
        b'{' | b'[' => {
            let e = end_of_container(b, i)?;
            let kind = if c == b'{' { Kind::Obj } else { Kind::Arr };
            Some((Val { kind, span: i..e }, e))
        }
        b'-' | b'0'..=b'9' => {
            let mut j = i + 1;
            while j < b.len() && matches!(b[j], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                j += 1;
            }
            Some((Val { kind: Kind::Num, span: i..j }, j))
        }
        b't' | b'f' | b'n' => {
            let lit: &[u8] = match c {
                b't' => b"true",
                b'f' => b"false",
                _ => b"null",
            };
            let e = i + lit.len();
            if b.get(i..e)? != lit {
                return None;
            }
            Some((Val { kind: Kind::Lit, span: i..e }, e))
        }
        _ => None,
    }
}

/// The whole of `b` is ONE object (whitespace around it allowed).
#[must_use]
pub fn is_one_object(b: &[u8]) -> bool {
    let i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return false;
    }
    match end_of_container(b, i) {
        Some(e) => skip_ws(b, e) == b.len(),
        None => false,
    }
}

/// Walk the top level of the object starting at `obj.start` (after
/// whitespace) calling `f(key, value)` per member until it returns
/// `false`. `None` when the object is malformed.
fn walk_members<F>(b: &[u8], start: usize, mut f: F) -> Option<()>
where
    F: FnMut(&[u8], &Val) -> bool,
{
    let mut i = skip_ws(b, start);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i = skip_ws(b, i + 1);
    if b.get(i) == Some(&b'}') {
        return Some(());
    }
    loop {
        if b.get(i) != Some(&b'"') {
            return None;
        }
        let ke = end_of_string(b, i)?;
        let key = &b[i + 1..ke - 1];
        i = skip_ws(b, ke);
        if b.get(i) != Some(&b':') {
            return None;
        }
        let (v, e) = value_at(b, i + 1)?;
        if !f(key, &v) {
            return Some(());
        }
        i = skip_ws(b, e);
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(b'}') => return Some(()),
            _ => return None,
        }
    }
}

/// The first top-level member named `key` of the object at the start
/// of `b` (spans index `b`).
#[must_use]
pub fn field(b: &[u8], key: &[u8]) -> Option<Val> {
    let mut hit = None;
    walk_members(b, 0, |k, v| {
        if k == key {
            hit = Some(v.clone());
            false
        } else {
            true
        }
    })?;
    hit
}

/// The top-level member named `key` of the object `obj` (a span of
/// `b`, as returned for a nested [`Kind::Obj`]); spans index `b`.
#[must_use]
pub fn field_in(b: &[u8], obj: &Val, key: &[u8]) -> Option<Val> {
    if obj.kind != Kind::Obj {
        return None;
    }
    let mut hit = None;
    walk_members(&b[..obj.span.end], obj.span.start, |k, v| {
        if k == key {
            hit = Some(v.clone());
            false
        } else {
            true
        }
    })?;
    hit
}

/// Like [`field_in`], but a key that appears TWICE is ambiguous and
/// reads as [`Malformed`] — the one a verdict is read from must be
/// unique.
///
/// # Errors
///
/// [`Malformed`] on a malformed object or a duplicated key.
pub fn field_unique_in(b: &[u8], obj: &Val, key: &[u8]) -> Result<Option<Val>, Malformed> {
    if obj.kind != Kind::Obj {
        return Err(Malformed);
    }
    let mut hit: Option<Val> = None;
    let mut dup = false;
    walk_members(&b[..obj.span.end], obj.span.start, |k, v| {
        if k == key {
            if hit.is_some() {
                dup = true;
                return false;
            }
            hit = Some(v.clone());
        }
        true
    })
    .ok_or(Malformed)?;
    if dup {
        return Err(Malformed);
    }
    Ok(hit)
}

/// The whole buffer as a [`Kind::Obj`] value (after [`is_one_object`]).
/// Its top level is walked in full, so a malformed member refuses here
/// rather than hiding behind the one key a caller happens to read.
#[must_use]
pub fn root(b: &[u8]) -> Option<Val> {
    let (v, e) = value_at(b, 0)?;
    if v.kind != Kind::Obj || skip_ws(b, e) != b.len() {
        return None;
    }
    walk_members(b, v.span.start, |_, _| true)?;
    Some(v)
}

/// The elements of an array value, in order.
pub struct Items<'a> {
    b: &'a [u8],
    i: usize,
    end: usize,
    first: bool,
    broken: bool,
}

/// Iterate the array `arr` (a [`Kind::Arr`] span of `b`).
#[must_use]
pub fn items<'a>(b: &'a [u8], arr: &Val) -> Items<'a> {
    let ok = arr.kind == Kind::Arr;
    Items {
        b,
        i: if ok { arr.span.start + 1 } else { 0 },
        end: if ok { arr.span.end - 1 } else { 0 },
        first: true,
        broken: !ok,
    }
}

impl Items<'_> {
    /// The next element; `Some(Err(Malformed))` once on a malformed
    /// array (then `None`).
    pub fn next_item(&mut self) -> Option<Result<Val, Malformed>> {
        if self.broken {
            return None;
        }
        let mut i = skip_ws(self.b, self.i);
        if i >= self.end {
            return None;
        }
        if !self.first {
            if self.b[i] != b',' {
                self.broken = true;
                return Some(Err(Malformed));
            }
            i += 1;
        }
        self.first = false;
        match value_at(&self.b[..self.end], i) {
            Some((v, e)) => {
                self.i = e;
                Some(Ok(v))
            }
            None => {
                self.broken = true;
                Some(Err(Malformed))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACK: &[u8] = br#"{"timestamp":1767225600000,"info":{"symbol":"BTC-20261002-100000-C","price":"0.0005","size":"0.000001","side":"Buy","tif":"gtc","is_perp":false,"order_id":7},"status":"OPEN","filled_size":"0","wallet_address":"0xab","order_id":42,"reason":null}"#;

    #[test]
    fn top_level_keys_are_read_and_nested_ones_are_not() {
        let r = root(ACK).unwrap();
        let oid = field_in(ACK, &r, b"order_id").unwrap();
        assert_eq!(oid.as_u64(ACK), Some(42), "the top-level id, not info's 7");
        let st = field(ACK, b"status").unwrap();
        assert_eq!(st.bytes(ACK), b"OPEN");
        assert!(field(ACK, b"reason").unwrap().is_null(ACK));
        let info = field(ACK, b"info").unwrap();
        assert_eq!(field_in(ACK, &info, b"order_id").unwrap().as_u64(ACK), Some(7));
        assert_eq!(field_in(ACK, &info, b"is_perp").unwrap().as_bool(ACK), Some(false));
        assert!(field(ACK, b"symbol").is_none(), "symbol lives inside info only");
    }

    #[test]
    fn a_duplicated_verdict_key_is_ambiguous() {
        let b = br#"{"status":"OPEN","x":1,"status":"REJECTED"}"#;
        let r = root(b).unwrap();
        assert_eq!(field_unique_in(b, &r, b"status"), Err(Malformed));
        assert!(field_unique_in(b, &r, b"x").unwrap().is_some());
        assert_eq!(field_unique_in(b, &r, b"nope"), Ok(None));
    }

    #[test]
    fn strings_with_escapes_and_braces_do_not_confuse_depth() {
        let b = br#"{"reason":"bad \"}\" order_id A","status":"REJECTED"}"#;
        let st = field(b, b"status").unwrap();
        assert_eq!(st.bytes(b), b"REJECTED");
        assert!(field(b, b"order_id").is_none());
    }

    #[test]
    fn arrays_iterate_in_order_and_a_broken_one_says_so() {
        let b = br#"{"data":[{"a":1},{"a":2} , {"a":3}],"bad":[1 2]}"#;
        let d = field(b, b"data").unwrap();
        let mut it = items(b, &d);
        let mut seen = [0u64; 3];
        let mut n = 0usize;
        while let Some(Ok(v)) = it.next_item() {
            seen[n] = field_in(b, &v, b"a").unwrap().as_u64(b).unwrap();
            n += 1;
        }
        assert_eq!(seen, [1, 2, 3]);
        let bad = field(b, b"bad").unwrap();
        let mut it = items(b, &bad);
        assert!(matches!(it.next_item(), Some(Ok(_))));
        assert_eq!(it.next_item(), Some(Err(Malformed)));
        assert_eq!(it.next_item(), None);
    }

    #[test]
    fn junk_and_truncation_are_refused() {
        for bad in [
            &b""[..],
            b"[]",
            b"{",
            b"{\"a\":}",
            b"{\"a\":1,}",
            b"{\"a\" 1}",
            b"{\"a\":tru}",
            b"{\"a\":\"x}",
            b"{\"a\":1} trailing",
        ] {
            assert!(root(bad).is_none(), "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(is_one_object(b" {\"a\":[1,{\"b\":\"}\"}]} \n"));
        assert!(!is_one_object(b"{} {}"));
    }
}
