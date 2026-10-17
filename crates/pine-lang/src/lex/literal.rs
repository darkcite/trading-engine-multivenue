// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Numbers, colours, single-line and multiline strings (§4.4); the crate's one copy (§4.13).

use pine_ir::Dialect;

use super::token::{TokKind, Token, HAS_ALPHA, STR_FORM_SHIFT};
use super::{word_continue, Lexer};
use crate::code;
use crate::policy::{MultilineEdges, UnknownEscape};

impl Lexer<'_, '_> {
    /// A number at `i` (a digit, or `.` before a digit).
    pub(super) fn number(&mut self, i: usize) -> usize {
        let b = self.b;
        let n = b.len();
        let mut j = i;
        let mut float = false;
        if b[i] == b'.' {
            // L2: `.5`.
            j = digits(b, i + 1);
            float = true;
            match exponent(b, j) {
                Exp::Digits(k) => j = k,
                Exp::Bad(k) => return self.bad_number(i, k),
                Exp::None => {}
            }
        } else {
            j = digits(b, j);
            if j < n && b[j] == b'.' {
                let d = if j + 1 < n { b[j + 1] } else { 0 };
                if d.is_ascii_digit() {
                    j = digits(b, j + 1);
                    float = true;
                    match exponent(b, j) {
                        Exp::Digits(k) => j = k,
                        Exp::Bad(k) => return self.bad_number(i, k),
                        Exp::None => {}
                    }
                } else if let Exp::Digits(k) = exponent(b, j + 1) {
                    // `1.e5`.
                    j = k;
                    float = true;
                } else if super::word_start(self.src, j + 1) == 0 {
                    // L2: `1.` not followed by a digit, an exponent or a word start.
                    j += 1;
                    float = true;
                }
                // else `1.abs`: `Int` then `Dot`.
            } else {
                match exponent(b, j) {
                    Exp::Digits(k) => {
                        j = k;
                        float = true;
                    }
                    Exp::Bad(k) => return self.bad_number(i, k),
                    Exp::None => {}
                }
            }
        }
        // A digit run continued by a word character, or a fraction followed by `.digit`.
        let dot_digit = j + 1 < n && b[j] == b'.' && b[j + 1].is_ascii_digit();
        if word_continue(self.src, j) > 0 || (float && dot_digit) {
            return self.bad_number(i, j);
        }
        let text = &self.src[i..j];
        if float {
            match text.parse::<f64>() {
                Ok(v) if v.is_finite() => {}
                _ => {
                    return self.error(
                        code::NUMBER_OUT_OF_RANGE,
                        i,
                        j,
                        "a float literal out of range",
                    )
                }
            }
            self.push(Token::new(TokKind::Float, i as u32, j as u32));
        } else {
            if int_value(text.as_bytes()).is_none() {
                return self.error(
                    code::NUMBER_OUT_OF_RANGE,
                    i,
                    j,
                    "an integer literal above 18446744073709551615",
                );
            }
            self.push(Token::new(TokKind::Int, i as u32, j as u32));
        }
        j
    }

    /// 1004 over the maximal run of digits, letters, `_` and `.` from `i` (at least to `j`).
    fn bad_number(&mut self, i: usize, j: usize) -> usize {
        let b = self.b;
        let mut k = j;
        loop {
            if k < b.len() && b[k] == b'.' {
                k += 1;
                continue;
            }
            let w = word_continue(self.src, k);
            if w == 0 {
                break;
            }
            k += w;
        }
        self.error(code::BAD_NUMBER_LITERAL, i, k, "a malformed number literal")
    }

    /// A colour at `i` (`#`): exactly 6 or 8 hex digits not followed by a word character.
    pub(super) fn color(&mut self, i: usize) -> usize {
        let b = self.b;
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_hexdigit() {
            j += 1;
        }
        let h = j - i - 1;
        if (h == 6 || h == 8) && word_continue(self.src, j) == 0 {
            let mut t = Token::new(TokKind::Color, i as u32, j as u32);
            if h == 8 {
                t.flags |= HAS_ALPHA;
            }
            self.push(t);
            return j;
        }
        let mut k = i + 1;
        loop {
            let w = word_continue(self.src, k);
            if w == 0 {
                break;
            }
            k += w;
        }
        self.error(
            code::BAD_COLOR_LITERAL,
            i,
            k,
            "a colour literal is `#` and exactly 6 or 8 hex digits",
        )
    }

    /// A string at `i` (a quote): single-line, or multiline when the quote is tripled.
    pub(super) fn string(&mut self, i: usize) -> usize {
        let b = self.b;
        let n = b.len();
        let q = b[i];
        if i + 2 < n && b[i + 1] == q && b[i + 2] == q {
            return self.multiline(i);
        }
        let mut j = i + 1;
        loop {
            if j < n && (b[j] == b'\n' || b[j] == b'\r') && self.pol.string_line_wrap {
                // pin:01-string-line-wrap accepted: the break stays in the value, normalised.
                j += 1;
                continue;
            }
            if j >= n || b[j] == b'\n' || b[j] == b'\r' {
                let msg = if self.pol.string_line_wrap {
                    "a string literal is not closed before the end of the file"
                } else {
                    "a string literal is not closed before the end of its line"
                };
                return self.error(code::UNTERMINATED_STRING, i, j, msg);
            }
            if b[j] == b'\\' {
                if j + 1 < n && b[j + 1] != b'\n' && b[j + 1] != b'\r' {
                    j += 2;
                } else {
                    j += 1;
                }
                continue;
            }
            if b[j] == q {
                break;
            }
            j += 1;
        }
        let form = if q == b'"' { 0 } else { 1 };
        self.push_str_token(i, j + 1, i + 1, j, form);
        j + 1
    }

    /// A multiline string at `i` (a tripled quote); v6-only (SD3), lexed in v5 all the same.
    fn multiline(&mut self, i: usize) -> usize {
        let b = self.b;
        let n = b.len();
        let q = b[i];
        let mut j = i + 3;
        loop {
            if j >= n {
                return self.error(
                    code::UNTERMINATED_STRING,
                    i,
                    n,
                    "a multiline string literal is not closed before the end of the file",
                );
            }
            match b[j] {
                b'\\' => {
                    j += 2;
                    continue;
                }
                x if x == q && j + 2 < n && b[j + 1] == q && b[j + 2] == q => break,
                _ => {}
            }
            j += 1;
        }
        let hi = j + 3;
        if self.dialect == Dialect::V5 {
            self.diags.at(
                code::CONSTRUCT_NOT_IN_DIALECT,
                self.file,
                i as u32,
                hi as u32,
                "multiline string literals are v6-only",
            );
        }
        let (lo_v, hi_v) = multiline_value_range(b, i + 3, j, self.pol.multiline_edges);
        let form = if q == b'"' { 2 } else { 3 };
        self.push_str_token(i, hi, lo_v, hi_v, form);
        hi
    }

    /// Push a `Str` token over `[lo, hi)` whose raw value is `src[lo_v..hi_v]`, through the
    /// scratch exactly when `needs_scratch` says so (§4.13).
    fn push_str_token(&mut self, lo: usize, hi: usize, lo_v: usize, hi_v: usize, form: u8) {
        let raw = &self.src[lo_v..hi_v];
        let sym = if needs_scratch(raw.as_bytes()) {
            resolve_into(raw, self.pol.unknown_escape, &mut self.scratch);
            let value = core::str::from_utf8(&self.scratch).expect(
                "pine-lang bug: a string scratch holds only whole UTF-8 scalars of a &str and ASCII \
                 escape bytes",
            );
            self.syms.intern(value)
        } else {
            self.syms.intern(raw)
        };
        let mut t = Token::new(TokKind::Str, lo as u32, hi as u32);
        t.sym = sym;
        t.flags |= form << STR_FORM_SHIFT;
        self.push(t);
    }
}

/// A literal's raw value needs the §4.13 scratch copy exactly when it holds a `\` (an escape) or a
/// `\r` (a line break to normalise); every other value is interned from the borrowed slice.
pub(crate) fn needs_scratch(raw: &[u8]) -> bool {
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] == b'\\' || raw[i] == b'\r' {
            return true;
        }
        i += 1;
    }
    false
}

/// Escape-process and newline-normalise `raw` into `scratch` (§4.4 strings). Every byte appended
/// is either a run of `raw` (whole UTF-8 scalars, split only at ASCII bytes) or an ASCII byte, so
/// the scratch is valid UTF-8.
fn resolve_into(raw: &str, unknown: UnknownEscape, scratch: &mut Vec<u8>) {
    let r = raw.as_bytes();
    let n = r.len();
    scratch.clear();
    let mut run = 0usize;
    let mut k = 0usize;
    while k < n {
        match r[k] {
            b'\\' if k + 1 < n => {
                let rep = match r[k + 1] {
                    b'n' => Some(b'\n'),
                    b't' => Some(b'\t'),
                    b'\\' => Some(b'\\'),
                    b'"' => Some(b'"'),
                    b'\'' => Some(b'\''),
                    _ => None,
                };
                append(scratch, &r[run..k]);
                match rep {
                    Some(c) => {
                        scratch.push(c);
                        k += 2;
                    }
                    None => {
                        // pin:01-unknown-escape: the next character is kept by the run either way;
                        // the backslash is dropped (Z017: `str.length("a\qb")` is 3).
                        if unknown == UnknownEscape::KeepBoth {
                            scratch.push(b'\\');
                        }
                        k += 1;
                    }
                }
                run = k;
            }
            b'\r' => {
                append(scratch, &r[run..k]);
                scratch.push(b'\n');
                k += 1;
                if k < n && r[k] == b'\n' {
                    k += 1;
                }
                run = k;
            }
            _ => k += 1,
        }
    }
    append(scratch, &r[run..n]);
}

/// Append one unchanged run of a literal's source bytes to the scratch.
fn append(scratch: &mut Vec<u8>, run: &[u8]) {
    // COPY: a string literal's escape-processed / newline-normalised value into the lexer's reused
    // scratch, ≤ the literal's length, once per such literal at load — the value SymTable must hold
    // (the CONST string region) does not exist contiguously in the source — interning the raw text
    // and unescaping in sema rejected: it moves the same copy downstream and puts the wrong bytes
    // in CONST.
    scratch.extend_from_slice(run);
}

/// The raw value range of a multiline literal whose content is `b[a..z)`
/// (pin:01-multiline-string-value).
fn multiline_value_range(b: &[u8], a: usize, z: usize, edges: MultilineEdges) -> (usize, usize) {
    let mut lo = a;
    let mut hi = z;
    // pin:01-multiline-string-value `Verbatim` (Z018: `"""ab` · `    """` is 7 on TradingView):
    // every byte between the delimiters.
    if edges == MultilineEdges::Verbatim {
        return (lo, hi);
    }
    // Otherwise the closing line's indentation is not part of the value.
    let mut k = hi;
    while k > lo && (b[k - 1] == b' ' || b[k - 1] == b'\t') {
        k -= 1;
    }
    if k > lo && (b[k - 1] == b'\n' || b[k - 1] == b'\r') {
        hi = k;
    }
    if edges == MultilineEdges::TrimBreaks {
        if lo < hi && b[lo] == b'\r' {
            lo += 1;
            if lo < hi && b[lo] == b'\n' {
                lo += 1;
            }
        } else if lo < hi && b[lo] == b'\n' {
            lo += 1;
        }
        if hi > lo && b[hi - 1] == b'\n' {
            hi -= 1;
            if hi > lo && b[hi - 1] == b'\r' {
                hi -= 1;
            }
        } else if hi > lo && b[hi - 1] == b'\r' {
            hi -= 1;
        }
    }
    (lo, hi)
}

/// The index past the ASCII digit run starting at `i`.
fn digits(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    i
}

/// An exponent attempt at `i`.
enum Exp {
    /// No `e`/`E` at `i`.
    None,
    /// `[eE][+-]?D+`, ending at the index.
    Digits(usize),
    /// `e`/`E` (and a sign) without digits, ending at the index.
    Bad(usize),
}

/// Scan `[eE][+-]?D+` at `i`.
fn exponent(b: &[u8], i: usize) -> Exp {
    let n = b.len();
    if i >= n || (b[i] != b'e' && b[i] != b'E') {
        return Exp::None;
    }
    let mut j = i + 1;
    if j < n && (b[j] == b'+' || b[j] == b'-') {
        j += 1;
    }
    let k = digits(b, j);
    if k == j {
        Exp::Bad(j)
    } else {
        Exp::Digits(k)
    }
}

/// The value of a decimal digit run, `None` above `u64::MAX`. The parser reads it the same way.
pub(crate) fn int_value(d: &[u8]) -> Option<u64> {
    let mut v: u64 = 0;
    let mut i = 0usize;
    while i < d.len() {
        v = v.checked_mul(10)?.checked_add(u64::from(d[i] - b'0'))?;
        i += 1;
    }
    Some(v)
}
