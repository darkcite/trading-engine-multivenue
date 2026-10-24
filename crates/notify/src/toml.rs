// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The TOML subset `notify.toml` is written in: comments, `[section]`,
//! `[[array.table]]`, `key = value` with basic or literal strings, integers
//! (underscores allowed), booleans and single- or multi-line arrays of
//! those. Anything else is refused with its line number — the example file
//! (`notify.toml.example`) is the parser's contract, as every house
//! `*.toml.example` is.

use std::collections::BTreeMap;

/// A value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TVal {
    /// A string.
    Str(String),
    /// An integer.
    Int(i64),
    /// A boolean.
    Bool(bool),
    /// An array of values.
    Arr(Vec<TVal>),
}

/// One table: keys with their line numbers, in source order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    /// `(key, value, line)`.
    pub entries: Vec<(String, TVal, usize)>,
}

impl Table {
    /// The value of `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&TVal> {
        self.entries
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, v, _)| v)
    }
}

/// A parsed document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    /// Keys before any header.
    pub root: Table,
    /// `[name]` tables.
    pub sections: BTreeMap<String, Table>,
    /// `[[name]]` arrays of tables.
    pub arrays: BTreeMap<String, Vec<Table>>,
}

/// A parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlErr {
    /// 1-based line.
    pub line: usize,
    /// What was wrong.
    pub what: String,
}

impl std::fmt::Display for TomlErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.what)
    }
}

fn err(line: usize, what: impl Into<String>) -> TomlErr {
    TomlErr {
        line,
        what: what.into(),
    }
}

enum Target {
    Root,
    Section(String),
    Array(String),
}

/// Strip a `#` comment that is not inside a string.
pub(crate) fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let mut in_basic = false;
    let mut in_lit = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if in_basic => i += 1,
            b'"' if !in_lit => in_basic = !in_basic,
            b'\'' if !in_basic => in_lit = !in_lit,
            b'#' if !in_basic && !in_lit => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// Count unquoted `[` minus `]` (to join multi-line arrays).
fn bracket_balance(s: &str) -> i64 {
    let b = s.as_bytes();
    let (mut in_basic, mut in_lit, mut n) = (false, false, 0i64);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if in_basic => i += 1,
            b'"' if !in_lit => in_basic = !in_basic,
            b'\'' if !in_basic => in_lit = !in_lit,
            b'[' if !in_basic && !in_lit => n += 1,
            b']' if !in_basic && !in_lit => n -= 1,
            _ => {}
        }
        i += 1;
    }
    n
}

fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Parse a document.
///
/// # Errors
/// [`TomlErr`] naming the first offending line.
pub fn parse(src: &str) -> Result<Doc, TomlErr> {
    let mut doc = Doc::default();
    let mut target = Target::Root;
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let lineno = i + 1;
        let line = strip_comment(lines[i]).trim();
        i += 1;
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("[[") {
            let name = rest
                .strip_suffix("]]")
                .ok_or_else(|| err(lineno, "unclosed [[table]]"))?
                .trim();
            if !name.split('.').all(valid_key) {
                return Err(err(lineno, format!("bad table name '{name}'")));
            }
            doc.arrays
                .entry(name.to_string())
                .or_default()
                .push(Table::default());
            target = Target::Array(name.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest
                .strip_suffix(']')
                .ok_or_else(|| err(lineno, "unclosed [table]"))?
                .trim();
            if !name.split('.').all(valid_key) {
                return Err(err(lineno, format!("bad table name '{name}'")));
            }
            if doc.sections.contains_key(name) {
                return Err(err(lineno, format!("table [{name}] defined twice")));
            }
            doc.sections.insert(name.to_string(), Table::default());
            target = Target::Section(name.to_string());
            continue;
        }
        let Some(eq) = line.find('=') else {
            return Err(err(lineno, "expected 'key = value'"));
        };
        let key = line[..eq].trim();
        if !valid_key(key) {
            return Err(err(lineno, format!("bad key '{key}'")));
        }
        let mut raw = line[eq + 1..].trim().to_string();
        // A multi-line array: join until the brackets balance.
        while bracket_balance(&raw) > 0 {
            if i >= lines.len() {
                return Err(err(lineno, "unclosed array"));
            }
            raw.push(' ');
            raw.push_str(strip_comment(lines[i]).trim());
            i += 1;
        }
        let (v, rest) = value(raw.as_str(), lineno)?;
        if !rest.trim().is_empty() {
            return Err(err(lineno, format!("trailing text '{}'", rest.trim())));
        }
        let table = match &target {
            Target::Root => &mut doc.root,
            Target::Section(n) => doc
                .sections
                .get_mut(n)
                .ok_or_else(|| err(lineno, "lost table"))?,
            Target::Array(n) => doc
                .arrays
                .get_mut(n)
                .and_then(|v| v.last_mut())
                .ok_or_else(|| err(lineno, "lost table"))?,
        };
        if table.get(key).is_some() {
            return Err(err(lineno, format!("key '{key}' set twice")));
        }
        table.entries.push((key.to_string(), v, lineno));
    }
    Ok(doc)
}

/// Parse one value at the start of `s`; returns it and the rest.
fn value(s: &str, line: usize) -> Result<(TVal, &str), TomlErr> {
    let s = s.trim_start();
    let b = s.as_bytes();
    match b.first() {
        None => Err(err(line, "missing value")),
        Some(b'"') => {
            let mut out = String::new();
            let mut chars = s[1..].char_indices();
            while let Some((j, c)) = chars.next() {
                match c {
                    '"' => return Ok((TVal::Str(out), &s[j + 2..])),
                    '\\' => match chars.next() {
                        Some((_, 'n')) => out.push('\n'),
                        Some((_, 't')) => out.push('\t'),
                        Some((_, '"')) => out.push('"'),
                        Some((_, '\\')) => out.push('\\'),
                        _ => return Err(err(line, "unsupported escape in string")),
                    },
                    c => out.push(c),
                }
            }
            Err(err(line, "unterminated string"))
        }
        Some(b'\'') => {
            let end = s[1..]
                .find('\'')
                .ok_or_else(|| err(line, "unterminated string"))?;
            Ok((TVal::Str(s[1..=end].to_string()), &s[end + 2..]))
        }
        Some(b'[') => {
            let mut items = Vec::new();
            let mut rest = s[1..].trim_start();
            loop {
                if let Some(r) = rest.strip_prefix(']') {
                    return Ok((TVal::Arr(items), r));
                }
                let (v, r) = value(rest, line)?;
                items.push(v);
                rest = r.trim_start();
                if let Some(r) = rest.strip_prefix(',') {
                    rest = r.trim_start();
                } else if !rest.starts_with(']') {
                    return Err(err(line, "expected ',' or ']' in array"));
                }
            }
        }
        Some(_) => {
            let end = s.find([',', ']', ' ', '\t']).unwrap_or(s.len());
            let tok = &s[..end];
            let v = match tok {
                "true" => TVal::Bool(true),
                "false" => TVal::Bool(false),
                _ => {
                    let clean: String = tok.chars().filter(|&c| c != '_').collect();
                    let n = clean.parse::<i64>().map_err(|_| {
                        err(
                            line,
                            format!("unsupported value '{tok}' (strings need quotes)"),
                        )
                    })?;
                    TVal::Int(n)
                }
            };
            Ok((v, &s[end..]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_subset() {
        let src = r#"
# a comment
tag = "SA"   # trailing comment with "quotes"
n = -1_000
on = true
list = [-250, -400]
paths = [
  "~/a",   # one
  '/b#c',
]
[sec]
x = 'lit'
[[slot]]
slot = 3
[[slot]]
slot = 6
live_only = false
"#;
        let d = parse(src).unwrap();
        assert_eq!(d.root.get("tag"), Some(&TVal::Str("SA".into())));
        assert_eq!(d.root.get("n"), Some(&TVal::Int(-1000)));
        assert_eq!(d.root.get("on"), Some(&TVal::Bool(true)));
        assert_eq!(
            d.root.get("list"),
            Some(&TVal::Arr(vec![TVal::Int(-250), TVal::Int(-400)]))
        );
        assert_eq!(
            d.root.get("paths"),
            Some(&TVal::Arr(vec![
                TVal::Str("~/a".into()),
                TVal::Str("/b#c".into())
            ]))
        );
        assert_eq!(d.sections["sec"].get("x"), Some(&TVal::Str("lit".into())));
        assert_eq!(d.arrays["slot"].len(), 2);
        assert_eq!(
            d.arrays["slot"][1].get("live_only"),
            Some(&TVal::Bool(false))
        );
    }

    #[test]
    fn refuses_what_it_does_not_know() {
        for bad in [
            "x = 1.5",
            "x = bare",
            "x = \"open",
            "= 1",
            "x 1",
            "[a\nx=1",
            "x = 1\nx = 2",
            "[a]\n[a]",
            "x = [1, 2",
            "x = 1 2",
            "x = { a = 1 }",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let e = parse("a = 1\nb = what").unwrap_err();
        assert_eq!(e.line, 2);
    }
}
