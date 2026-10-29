// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! A hand-written JSON Lines writer and flat-row reader (impl-09 §3.5.1). No `serde`, no
//! `serde_json` (CLAUDE.md). A row is one flat object; a value is a string, an integer
//! (`i64` or `u64`), `true`/`false`, `null`, or an array of strings. Floats and nested objects
//! are refused by the reader (I20), so every value renders and re-reads exactly.

/// The longest line the reader accepts: a bound, never an allocation spiral.
pub const MAX_LINE: usize = 1 << 20;

/// One value of a flat row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A JSON string (unescaped).
    Str(String),
    /// An integer in `i64::MIN..=u64::MAX`.
    Int(i128),
    /// `true` / `false`.
    Bool(bool),
    /// `null`.
    Null,
    /// An array of strings.
    Arr(Vec<String>),
}

/// One parsed row: its keys in line order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Row {
    /// `(key, value)` in line order; keys are unique.
    pub fields: Vec<(String, Value)>,
}

impl Row {
    /// The value of `key`, if present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// Builds one line: `{"schema":1,...}\n`.
#[derive(Debug)]
pub struct Line<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> Line<'a> {
    /// Start a row in `out` (appended), with `"schema":1` first (I20).
    pub fn begin(out: &'a mut Vec<u8>) -> Self {
        out.extend_from_slice(b"{\"schema\":1");
        Line { out }
    }
    fn key(&mut self, k: &str) {
        self.out.push(b',');
        write_str(self.out, k);
        self.out.push(b':');
    }
    /// A string value.
    pub fn str(&mut self, k: &str, v: &str) -> &mut Self {
        self.key(k);
        write_str(self.out, v);
        self
    }
    /// A nullable string value.
    pub fn opt_str(&mut self, k: &str, v: Option<&str>) -> &mut Self {
        match v {
            Some(s) => self.str(k, s),
            None => self.null(k),
        }
    }
    /// An unsigned integer.
    pub fn u64(&mut self, k: &str, v: u64) -> &mut Self {
        self.key(k);
        self.out.extend_from_slice(v.to_string().as_bytes());
        self
    }
    /// A signed integer.
    pub fn i64(&mut self, k: &str, v: i64) -> &mut Self {
        self.key(k);
        self.out.extend_from_slice(v.to_string().as_bytes());
        self
    }
    /// A nullable unsigned integer.
    pub fn opt_u64(&mut self, k: &str, v: Option<u64>) -> &mut Self {
        match v {
            Some(x) => self.u64(k, x),
            None => self.null(k),
        }
    }
    /// A nullable signed integer.
    pub fn opt_i64(&mut self, k: &str, v: Option<i64>) -> &mut Self {
        match v {
            Some(x) => self.i64(k, x),
            None => self.null(k),
        }
    }
    /// A bool.
    pub fn bool(&mut self, k: &str, v: bool) -> &mut Self {
        self.key(k);
        self.out
            .extend_from_slice(if v { b"true" as &[u8] } else { b"false" });
        self
    }
    /// A nullable bool.
    pub fn opt_bool(&mut self, k: &str, v: Option<bool>) -> &mut Self {
        match v {
            Some(x) => self.bool(k, x),
            None => self.null(k),
        }
    }
    /// `null`.
    pub fn null(&mut self, k: &str) -> &mut Self {
        self.key(k);
        self.out.extend_from_slice(b"null");
        self
    }
    /// A `hex64`.
    pub fn hex(&mut self, k: &str, v: &[u8; 32]) -> &mut Self {
        let h = crate::hex64(v);
        self.str(k, &h)
    }
    /// A nullable `hex64`.
    pub fn opt_hex(&mut self, k: &str, v: Option<&[u8; 32]>) -> &mut Self {
        match v {
            Some(h) => self.hex(k, h),
            None => self.null(k),
        }
    }
    /// An array of strings.
    pub fn arr(&mut self, k: &str, v: &[String]) -> &mut Self {
        self.key(k);
        self.out.push(b'[');
        for (i, s) in v.iter().enumerate() {
            if i > 0 {
                self.out.push(b',');
            }
            write_str(self.out, s);
        }
        self.out.push(b']');
        self
    }
    /// Close the row with `}\n`.
    pub fn end(&mut self) {
        self.out.extend_from_slice(b"}\n");
    }
}

/// Write `s` as a JSON string: `"` and `\` escaped, control characters as `\n`, `\t`, `\r`,
/// `\b`, `\f` or `\u00XX`; everything else (non-ASCII included) passed through as UTF-8.
pub fn write_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            c if c < 0x20 || c == 0x7F => {
                const D: &[u8; 16] = b"0123456789abcdef";
                out.extend_from_slice(b"\\u00");
                out.push(D[(c >> 4) as usize]);
                out.push(D[(c & 15) as usize]);
            }
            c => out.push(c),
        }
    }
    out.push(b'"');
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t') {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn lit(&mut self, s: &[u8]) -> bool {
        if self.b.len() >= self.i + s.len() && &self.b[self.i..self.i + s.len()] == s {
            self.i += s.len();
            true
        } else {
            false
        }
    }
    fn hex4(&mut self) -> Result<u32, &'static str> {
        if self.i + 4 > self.b.len() {
            return Err("truncated \\u escape");
        }
        let mut v = 0u32;
        for k in 0..4 {
            let c = self.b[self.i + k];
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err("bad \\u escape"),
            };
            v = (v << 4) | u32::from(d);
        }
        self.i += 4;
        Ok(v)
    }
    fn string(&mut self) -> Result<String, &'static str> {
        if !self.eat(b'"') {
            return Err("expected a string");
        }
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return Err("unterminated string");
            };
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return Err("unterminated escape");
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                if !(self.eat(b'\\') && self.eat(b'u')) {
                                    return Err("lone surrogate");
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err("lone surrogate");
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err("lone surrogate");
                            } else {
                                hi
                            };
                            let ch = char::from_u32(cp).ok_or("bad code point")?;
                            let mut tmp = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                        }
                        _ => return Err("bad escape"),
                    }
                }
                c if c < 0x20 => return Err("raw control character in a string"),
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| "string is not UTF-8")
    }
    fn number(&mut self) -> Result<i128, &'static str> {
        let start = self.i;
        let neg = self.eat(b'-');
        let ds = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let digits = &self.b[ds..self.i];
        if digits.is_empty() {
            return Err("bad number");
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err("float refused (I20)");
        }
        if digits.len() > 1 && digits[0] == b'0' {
            return Err("leading zero");
        }
        if digits.len() > 20 {
            return Err("integer out of range");
        }
        let mut v: i128 = 0;
        for &d in digits {
            v = v * 10 + i128::from(d - b'0');
        }
        let v = if neg { -v } else { v };
        if v < i128::from(i64::MIN) || v > i128::from(u64::MAX) {
            return Err("integer out of range");
        }
        let _ = start;
        Ok(v)
    }
    fn value(&mut self) -> Result<Value, &'static str> {
        match self.peek() {
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b'-' | b'0'..=b'9') => Ok(Value::Int(self.number()?)),
            Some(b't') if self.lit(b"true") => Ok(Value::Bool(true)),
            Some(b'f') if self.lit(b"false") => Ok(Value::Bool(false)),
            Some(b'n') if self.lit(b"null") => Ok(Value::Null),
            Some(b'{') => Err("nested object refused (I20)"),
            Some(b'[') => {
                self.i += 1;
                let mut v = Vec::new();
                self.ws();
                if self.eat(b']') {
                    return Ok(Value::Arr(v));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return Err("array of non-strings refused");
                    }
                    v.push(self.string()?);
                    self.ws();
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b']') {
                        return Ok(Value::Arr(v));
                    }
                    return Err("bad array");
                }
            }
            _ => Err("bad value"),
        }
    }
}

/// Parse one line (without or with its trailing `\n`) into a flat row. Never panics.
pub fn parse_line(line: &[u8]) -> Result<Row, &'static str> {
    if line.len() > MAX_LINE {
        return Err("line over 1 MiB");
    }
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let mut p = P { b: line, i: 0 };
    p.ws();
    if !p.eat(b'{') {
        return Err("expected an object");
    }
    let mut row = Row::default();
    p.ws();
    if !p.eat(b'}') {
        loop {
            p.ws();
            let k = p.string()?;
            p.ws();
            if !p.eat(b':') {
                return Err("expected ':'");
            }
            p.ws();
            let v = p.value()?;
            if row.fields.iter().any(|(x, _)| *x == k) {
                return Err("duplicate key");
            }
            row.fields.push((k, v));
            p.ws();
            if p.eat(b',') {
                continue;
            }
            if p.eat(b'}') {
                break;
            }
            return Err("expected ',' or '}'");
        }
    }
    p.ws();
    if p.i != line.len() {
        return Err("trailing bytes after the object");
    }
    Ok(row)
}

/// Read every line of a JSONL file's bytes. A **final** line that does not parse is discarded
/// (a crash in mid-append, I7); any other bad line is `Err((1-based line, what))`. Empty input
/// gives no rows. Returns `(line_no, row)` pairs.
pub fn read_lines(bytes: &[u8]) -> Result<Vec<(u64, Row)>, (u64, &'static str)> {
    let mut out = Vec::new();
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    // `split` yields a trailing empty slice after a final `\n`.
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let n = lines.len();
    let complete_tail = bytes.last() == Some(&b'\n');
    for (i, l) in lines.iter().enumerate() {
        let last = i + 1 == n;
        match parse_line(l) {
            Ok(r) if !(last && !complete_tail && r.fields.is_empty()) => {
                out.push((i as u64 + 1, r));
            }
            Ok(_) => {}
            Err(e) => {
                if last {
                    break;
                }
                return Err((i as u64 + 1, e));
            }
        }
    }
    Ok(out)
}
