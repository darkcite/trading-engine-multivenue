// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A small JSON reader (into a tree) and a string escaper — enough for
//! `/state` (≈ 140 KB once a poll) and Telegram's answers. Handwritten:
//! `serde_json` is a forbidden crate in this workspace. Numbers are kept
//! as their source text so 64-bit integers stay exact; `/state` spells
//! nanosecond fields as strings (`u64_str`), and [`Json::as_i64`] reads
//! both forms.

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number, as written.
    Num(String),
    /// A string, unescaped.
    Str(String),
    /// An array.
    Arr(Vec<Json>),
    /// An object, in source order.
    Obj(Vec<(String, Json)>),
}

/// Why a document did not parse (byte offset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonErr {
    /// Byte offset of the failure.
    pub at: usize,
    /// What was wrong.
    pub what: &'static str,
}

impl std::fmt::Display for JsonErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "json: {} at byte {}", self.what, self.at)
    }
}

const MAX_DEPTH: usize = 64;

impl Json {
    /// Parse a whole document; trailing bytes other than whitespace fail.
    ///
    /// # Errors
    /// [`JsonErr`] on malformed input or nesting deeper than 64.
    pub fn parse(src: &[u8]) -> Result<Json, JsonErr> {
        let mut p = Parser { s: src, i: 0 };
        p.ws();
        let v = p.value(0)?;
        p.ws();
        if p.i != src.len() {
            return Err(p.err("trailing bytes"));
        }
        Ok(v)
    }

    /// Member `key` of an object.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Follow a dotted path of object keys.
    #[must_use]
    pub fn path(&self, dotted: &str) -> Option<&Json> {
        let mut cur = self;
        for k in dotted.split('.') {
            cur = cur.get(k)?;
        }
        Some(cur)
    }

    /// The elements of an array (empty for anything else).
    #[must_use]
    pub fn items(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            _ => &[],
        }
    }

    /// The members of an object (empty for anything else).
    #[must_use]
    pub fn members(&self) -> &[(String, Json)] {
        match self {
            Json::Obj(m) => m,
            _ => &[],
        }
    }

    /// An integer from a number or a numeric string; a fraction is cut.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        let t = match self {
            Json::Num(t) | Json::Str(t) => t.as_str(),
            Json::Bool(b) => return Some(i64::from(*b)),
            _ => return None,
        };
        let int = t.split(['.', 'e', 'E']).next().unwrap_or("");
        int.parse::<i64>().ok()
    }

    /// A string value.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, what: &'static str) -> JsonErr {
        JsonErr { at: self.i, what }
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn lit(&mut self, word: &[u8], v: Json) -> Result<Json, JsonErr> {
        if self.s[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("bad literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, JsonErr> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.peek() {
            None => Err(self.err("unexpected end")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit(b"true", Json::Bool(true)),
            Some(b'f') => self.lit(b"false", Json::Bool(false)),
            Some(b'n') => self.lit(b"null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected byte")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, JsonErr> {
        self.i += 1;
        let mut m = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(m));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected key"));
            }
            let k = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':'"));
            }
            self.i += 1;
            self.ws();
            let v = self.value(depth + 1)?;
            m.push((k, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(m));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, JsonErr> {
        self.i += 1;
        let mut a = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(a));
        }
        loop {
            self.ws();
            a.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(a));
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn number(&mut self) -> Result<Json, JsonErr> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let mut seen = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                seen |= c.is_ascii_digit();
                self.i += 1;
            } else {
                break;
            }
        }
        if !seen {
            return Err(self.err("bad number"));
        }
        let t = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err("bad number"))?;
        Ok(Json::Num(t.to_string()))
    }

    fn hex4(&mut self) -> Result<u32, JsonErr> {
        if self.i + 4 > self.s.len() {
            return Err(self.err("short \\u escape"));
        }
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.s[self.i];
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(self.err("bad \\u escape")),
            };
            v = v * 16 + u32::from(d);
            self.i += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, JsonErr> {
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return Err(self.err("unterminated escape"));
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                if self.s[self.i..].starts_with(b"\\u") {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err(self.err("bad surrogate pair"));
                                    }
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    return Err(self.err("lone surrogate"));
                                }
                            }
                            let ch =
                                char::from_u32(cp).ok_or_else(|| self.err("bad code point"))?;
                            let mut b = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                c if c < 0x20 => return Err(self.err("control byte in string")),
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| self.err("string is not UTF-8"))
    }
}

/// Append `s` to `out` as a JSON string literal (quotes included).
pub fn escape_into(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_documents_and_reads_numbers_exactly() {
        let src = br#"{"v":3,"now":{"wall_ns":"1792846648013000000"},"a":[1,-2.5,true,null,"x\u00e9\n"],"o":{}}"#;
        let j = Json::parse(src).unwrap();
        assert_eq!(j.get("v").and_then(Json::as_i64), Some(3));
        assert_eq!(
            j.path("now.wall_ns").and_then(Json::as_i64),
            Some(1_792_846_648_013_000_000)
        );
        let a = j.get("a").unwrap().items();
        assert_eq!(a.len(), 5);
        assert_eq!(a[1].as_i64(), Some(-2));
        assert_eq!(a[2], Json::Bool(true));
        assert_eq!(a[3], Json::Null);
        assert_eq!(a[4].as_str(), Some("xé\n"));
        assert!(j.get("o").unwrap().members().is_empty());
        assert!(j.get("missing").is_none());
    }

    #[test]
    fn surrogate_pairs_decode() {
        let j = Json::parse(br#""\ud83d\ude00""#).unwrap();
        assert_eq!(j.as_str(), Some("\u{1F600}"));
        assert!(Json::parse(br#""\ud83d""#).is_err());
    }

    #[test]
    fn malformed_input_fails_with_an_offset() {
        for bad in [
            &b"{"[..],
            b"{\"a\" 1}",
            b"[1,]",
            b"tru",
            b"\"abc",
            b"{} x",
            b"",
            b"-",
            b"\"\x01\"",
        ] {
            assert!(Json::parse(bad).is_err(), "{:?}", std::str::from_utf8(bad));
        }
        let deep = "[".repeat(100) + &"]".repeat(100);
        assert!(Json::parse(deep.as_bytes()).is_err());
    }

    #[test]
    fn escape_round_trips_through_the_parser() {
        let s = "a\"b\\c\nd\te\u{1}é";
        let mut out = String::new();
        escape_into(s, &mut out);
        assert_eq!(Json::parse(out.as_bytes()).unwrap().as_str(), Some(s));
    }
}
