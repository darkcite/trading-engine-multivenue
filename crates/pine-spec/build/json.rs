// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: build script — runs once in cargo's build step and is linked into no binary;
//! every copy here is cold.
//!
//! A zero-dependency RFC 8259 reader over UTF-8 with byte offsets and an int/float distinction
//! (impl-00 §4.4). No BOM, nesting ≤ 64, every escape including `\uXXXX` surrogate pairs; a lone
//! surrogate, an unescaped control character, a trailing comma, a leading zero, a duplicate key
//! in one object or content after the value is an error at its byte offset.

use std::borrow::Cow;

/// Maximum nesting depth of arrays and objects.
pub const MAX_DEPTH: u32 = 64;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Val<'a> {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number with no `.`, `e` or `E`; fits `i64`.
    Int(i64),
    /// Any other number (`str::parse::<f64>`, correctly rounded).
    Num(f64),
    /// A string: borrowed unless it contains an escape.
    Str(Cow<'a, str>),
    /// An array.
    Arr(Vec<Node<'a>>),
    /// An object, keys in file order (unique).
    Obj(Vec<(Cow<'a, str>, Node<'a>)>),
}

/// A value and the byte offset of its first byte.
#[derive(Debug, Clone, PartialEq)]
pub struct Node<'a> {
    /// Byte offset of the value's first byte.
    pub off: u32,
    /// The value.
    pub v: Val<'a>,
}

/// A parse error at a byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonErr {
    /// Byte offset of the offending byte.
    pub off: u32,
    /// What is wrong.
    pub what: &'static str,
}

/// Parse one JSON document.
pub fn parse(src: &[u8]) -> Result<Node<'_>, JsonErr> {
    if src.len() >= 3 && src[0] == 0xEF && src[1] == 0xBB && src[2] == 0xBF {
        return Err(JsonErr {
            off: 0,
            what: "byte order mark",
        });
    }
    if src.len() > u32::MAX as usize {
        return Err(JsonErr {
            off: 0,
            what: "input larger than 4 GiB",
        });
    }
    let mut p = Parser { s: src, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i < src.len() {
        return Err(p.err("trailing content"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, what: &'static str) -> JsonErr {
        JsonErr {
            off: self.i as u32,
            what,
        }
    }

    fn at(off: usize, what: &'static str) -> JsonErr {
        JsonErr {
            off: off as u32,
            what,
        }
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn value(&mut self, depth: u32) -> Result<Node<'a>, JsonErr> {
        let off = self.i as u32;
        let v = match self.peek() {
            None => return Err(self.err("unexpected end of input")),
            Some(b'{') => self.object(depth + 1)?,
            Some(b'[') => self.array(depth + 1)?,
            Some(b'"') => Val::Str(self.string()?),
            Some(b't') => self.word(b"true", Val::Bool(true))?,
            Some(b'f') => self.word(b"false", Val::Bool(false))?,
            Some(b'n') => self.word(b"null", Val::Null)?,
            Some(b'-' | b'0'..=b'9') => self.number()?,
            Some(_) => return Err(self.err("unexpected byte")),
        };
        Ok(Node { off, v })
    }

    fn word(&mut self, w: &'static [u8], v: Val<'a>) -> Result<Val<'a>, JsonErr> {
        let mut k = 0usize;
        while k < w.len() {
            if self.s.get(self.i + k) != Some(&w[k]) {
                return Err(Self::at(self.i + k, "invalid literal"));
            }
            k += 1;
        }
        self.i += w.len();
        Ok(v)
    }

    fn digits(&mut self) -> usize {
        let a = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        self.i - a
    }

    fn number(&mut self) -> Result<Val<'a>, JsonErr> {
        let a = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.i += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err("leading zero"));
                }
            }
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.err("expected a digit")),
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            self.i += 1;
            float = true;
            if self.digits() == 0 {
                return Err(self.err("expected a digit after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            float = true;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.digits() == 0 {
                return Err(self.err("expected a digit in the exponent"));
            }
        }
        // The slice is ASCII by construction.
        let t = std::str::from_utf8(&self.s[a..self.i]).map_err(|_| Self::at(a, "bad number"))?;
        if float {
            t.parse::<f64>()
                .map(Val::Num)
                .map_err(|_| Self::at(a, "bad number"))
        } else {
            t.parse::<i64>()
                .map(Val::Int)
                .map_err(|_| Self::at(a, "integer out of i64 range"))
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonErr> {
        let mut v = 0u32;
        let mut k = 0usize;
        while k < 4 {
            let c = match self.s.get(self.i) {
                Some(&c) => c,
                None => return Err(self.err("unterminated \\u escape")),
            };
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(self.err("bad hex digit in \\u escape")),
            };
            v = v * 16 + d as u32;
            self.i += 1;
            k += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<Cow<'a, str>, JsonErr> {
        let open = self.i;
        self.i += 1;
        let start = self.i;
        let mut owned: Option<String> = None;
        let mut run = start;
        loop {
            let c = match self.s.get(self.i) {
                Some(&c) => c,
                None => return Err(Self::at(open, "unterminated string")),
            };
            match c {
                b'"' => {
                    let tail = utf8(self.s, run, self.i)?;
                    let out = match owned {
                        None => Cow::Borrowed(tail),
                        Some(mut o) => {
                            // COPY: build time — an escaped string is decoded into an owned buffer.
                            o.push_str(tail);
                            Cow::Owned(o)
                        }
                    };
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    let esc = self.i;
                    let o = owned.get_or_insert_with(String::new);
                    // COPY: build time — the run before an escape joins the decoded buffer.
                    o.push_str(utf8(self.s, run, esc)?);
                    self.i += 1;
                    let e = match self.s.get(self.i) {
                        Some(&e) => e,
                        None => return Err(Self::at(open, "unterminated string")),
                    };
                    self.i += 1;
                    match e {
                        b'"' => o.push('"'),
                        b'\\' => o.push('\\'),
                        b'/' => o.push('/'),
                        b'b' => o.push('\u{8}'),
                        b'f' => o.push('\u{c}'),
                        b'n' => o.push('\n'),
                        b'r' => o.push('\r'),
                        b't' => o.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                if self.s.get(self.i) != Some(&b'\\')
                                    || self.s.get(self.i + 1) != Some(&b'u')
                                {
                                    return Err(Self::at(esc, "lone surrogate"));
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(Self::at(esc, "lone surrogate"));
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err(Self::at(esc, "lone surrogate"));
                            } else {
                                hi
                            };
                            match char::from_u32(cp) {
                                Some(ch) => o.push(ch),
                                None => return Err(Self::at(esc, "invalid code point")),
                            }
                        }
                        _ => return Err(Self::at(esc, "bad escape")),
                    }
                    run = self.i;
                }
                0x00..=0x1F => return Err(self.err("unescaped control character")),
                _ => self.i += 1,
            }
        }
    }

    fn array(&mut self, depth: u32) -> Result<Val<'a>, JsonErr> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting deeper than 64"));
        }
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Val::Arr(out));
        }
        loop {
            self.ws();
            out.push(self.value(depth)?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    let comma = self.i;
                    self.i += 1;
                    self.ws();
                    if self.peek() == Some(b']') {
                        return Err(Self::at(comma, "trailing comma"));
                    }
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Val::Arr(out));
                }
                None => return Err(self.err("unexpected end of input")),
                Some(_) => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn object(&mut self, depth: u32) -> Result<Val<'a>, JsonErr> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting deeper than 64"));
        }
        self.i += 1;
        let mut out: Vec<(Cow<'a, str>, Node<'a>)> = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Val::Obj(out));
        }
        loop {
            self.ws();
            let koff = self.i;
            if self.peek() != Some(b'"') {
                return Err(self.err("expected a string key"));
            }
            let k = self.string()?;
            if out.iter().any(|(q, _)| *q == k) {
                return Err(Self::at(koff, "duplicate key"));
            }
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':'"));
            }
            self.i += 1;
            self.ws();
            let v = self.value(depth)?;
            out.push((k, v));
            self.ws();
            match self.peek() {
                Some(b',') => {
                    let comma = self.i;
                    self.i += 1;
                    self.ws();
                    if self.peek() == Some(b'}') {
                        return Err(Self::at(comma, "trailing comma"));
                    }
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Val::Obj(out));
                }
                None => return Err(self.err("unexpected end of input")),
                Some(_) => return Err(self.err("expected ',' or '}'")),
            }
        }
    }
}

/// `s[a..b]` as UTF-8, or an error at the first invalid byte.
fn utf8(s: &[u8], a: usize, b: usize) -> Result<&str, JsonErr> {
    std::str::from_utf8(&s[a..b]).map_err(|e| JsonErr {
        off: (a + e.valid_up_to()) as u32,
        what: "invalid UTF-8",
    })
}
