// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Rules table — loaded from `rule_parser.py`'s JSON-array output.

use std::io::{self, Read};
use std::path::Path;

use crate::tags::{Family, KEY_LEN};

/// One decoded rule row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Rule name (inline, max `KEY_LEN` bytes).
    name: [u8; KEY_LEN],
    name_len: u8,
    /// Family tag.
    pub family: Family,
    /// Expected edge in basis points.
    pub edge_bps: u32,
    /// Holding horizon in ms.
    pub horizon_ms: u32,
    /// Maximum dollar risk per fire.
    pub max_risk_usd: u32,
}

impl Rule {
    /// Borrow the rule name as an ASCII slice.
    #[inline]
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

/// Why a rule failed to parse.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RuleError {
    /// Top-level JSON wasn't an array.
    NotAnArray,
    /// A required field was missing.
    MissingField,
    /// An integer field was out of range or unparseable.
    BadInt,
    /// `name` exceeded `KEY_LEN`.
    NameTooLong,
    /// Table is at capacity.
    Full,
}

/// Preallocated fixed-size rules table.
#[derive(Debug)]
pub struct RulesTable<const N: usize> {
    rules: [Rule; N],
    count: u32,
}

impl<const N: usize> RulesTable<N> {
    /// Empty table.
    pub const fn empty() -> Self {
        const EMPTY_RULE: Rule = Rule {
            name: [0; KEY_LEN],
            name_len: 0,
            family: Family::Other,
            edge_bps: 0,
            horizon_ms: 0,
            max_risk_usd: 0,
        };
        Self {
            rules: [EMPTY_RULE; N],
            count: 0,
        }
    }

    /// Append one rule. Boot-time only.
    pub fn push(&mut self, r: Rule) -> Result<(), RuleError> {
        let n = self.count as usize;
        if n >= N {
            return Err(RuleError::Full);
        }
        self.rules[n] = r;
        self.count = self.count.wrapping_add(1);
        Ok(())
    }

    /// Populated rules as a slice.
    #[inline]
    pub fn slice(&self) -> &[Rule] {
        &self.rules[..self.count as usize]
    }

    /// Number of populated rules.
    #[inline]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the table has no rows.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Look up the first rule with the given name. O(N).
    pub fn lookup(&self, name: &[u8]) -> Option<&Rule> {
        let mut i = 0;
        let n = self.count as usize;
        while i < n {
            if self.rules[i].name() == name {
                return Some(&self.rules[i]);
            }
            i += 1;
        }
        None
    }

    /// Boot-time loader for the JSON-array shape emitted by
    /// `rule_parser.write_artifact`. Returns the populated table
    /// plus the number of array entries skipped due to malformed
    /// fields.
    ///
    /// We tolerate per-rule errors so a single bad entry doesn't
    /// take down the boot — but a fundamentally malformed file
    /// (no top-level array) is fatal.
    pub fn load_json(path: &Path) -> io::Result<(Self, usize)> {
        let mut buf = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut buf)?;
        // Find the opening '[' and closing ']' of the top-level array.
        let start = buf
            .iter()
            .position(|&b| b == b'[')
            .ok_or_else(|| io::Error::other("rules file: missing top-level '['"))?;
        let end = buf
            .iter()
            .rposition(|&b| b == b']')
            .ok_or_else(|| io::Error::other("rules file: missing top-level ']'"))?;
        if end <= start {
            return Err(io::Error::other("rules file: malformed array delimiters"));
        }

        let mut table: Self = RulesTable::empty();
        let mut skipped = 0usize;
        let array_body = &buf[start + 1..end];
        for chunk in split_top_level_objects(array_body) {
            match parse_rule_object(chunk) {
                Ok(r) => {
                    if table.push(r).is_err() {
                        skipped += 1;
                    }
                }
                Err(_) => {
                    skipped += 1;
                }
            }
        }
        Ok((table, skipped))
    }
}

impl<const N: usize> Default for RulesTable<N> {
    fn default() -> Self {
        Self::empty()
    }
}

/// Walk a JSON array body, yielding each `{...}` object's bytes.
fn split_top_level_objects(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut iter = TopLevelObjects { body, pos: 0 };
    core::iter::from_fn(move || iter.next())
}

struct TopLevelObjects<'a> {
    body: &'a [u8],
    pos: usize,
}

impl<'a> TopLevelObjects<'a> {
    fn next(&mut self) -> Option<&'a [u8]> {
        // Skip whitespace + commas.
        while self.pos < self.body.len() {
            let b = self.body[self.pos];
            if b.is_ascii_whitespace() || b == b',' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos >= self.body.len() {
            return None;
        }
        if self.body[self.pos] != b'{' {
            // Skip stray content until the next '{'.
            while self.pos < self.body.len() && self.body[self.pos] != b'{' {
                self.pos += 1;
            }
            if self.pos >= self.body.len() {
                return None;
            }
        }
        // Walk until matching '}' — honour quoted strings and
        // backslash escapes.
        let start = self.pos;
        let mut depth: i32 = 0;
        let mut in_string = false;
        while self.pos < self.body.len() {
            let b = self.body[self.pos];
            if in_string {
                if b == b'\\' {
                    self.pos += 2;
                    continue;
                } else if b == b'"' {
                    in_string = false;
                }
            } else if b == b'"' {
                in_string = true;
            } else if b == b'{' {
                depth += 1;
            } else if b == b'}' {
                depth -= 1;
                if depth == 0 {
                    self.pos += 1;
                    return Some(&self.body[start..self.pos]);
                }
            }
            self.pos += 1;
        }
        None
    }
}

fn parse_rule_object(buf: &[u8]) -> Result<Rule, RuleError> {
    let name =
        crate::tags::find_string_field_pub(buf, b"\"name\"").ok_or(RuleError::MissingField)?;
    if name.len() > KEY_LEN {
        return Err(RuleError::NameTooLong);
    }
    let family_str =
        crate::tags::find_string_field_pub(buf, b"\"family\"").ok_or(RuleError::MissingField)?;
    let family = Family::from_bytes(family_str);

    let edge =
        crate::tags::find_int_field_pub(buf, b"\"edge_bps\"").ok_or(RuleError::MissingField)?;
    let horizon =
        crate::tags::find_int_field_pub(buf, b"\"horizon_ms\"").ok_or(RuleError::MissingField)?;
    let risk =
        crate::tags::find_int_field_pub(buf, b"\"max_risk_usd\"").ok_or(RuleError::MissingField)?;

    let edge_bps = u32::try_from(edge).map_err(|_| RuleError::BadInt)?;
    let horizon_ms = u32::try_from(horizon).map_err(|_| RuleError::BadInt)?;
    let max_risk_usd = u32::try_from(risk).map_err(|_| RuleError::BadInt)?;

    let mut name_arr = [0u8; KEY_LEN];
    name_arr[..name.len()].copy_from_slice(name);
    Ok(Rule {
        name: name_arr,
        name_len: name.len() as u8,
        family,
        edge_bps,
        horizon_ms,
        max_risk_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn empty_table_is_empty() {
        let t: RulesTable<4> = RulesTable::empty();
        assert!(t.is_empty());
        assert_eq!(t.slice().len(), 0);
    }

    #[test]
    fn push_then_lookup_round_trips() {
        let mut t: RulesTable<4> = RulesTable::empty();
        let r = parse_rule_object(
            br#"{"name":"crypto_breakout","family":"crypto","trigger":"x","edge_bps":12,"horizon_ms":2000,"max_risk_usd":50}"#,
        )
        .unwrap();
        t.push(r).unwrap();
        let got = t.lookup(b"crypto_breakout").unwrap();
        assert_eq!(got.edge_bps, 12);
        assert_eq!(got.horizon_ms, 2000);
        assert_eq!(got.max_risk_usd, 50);
        assert_eq!(got.family, Family::Crypto);
    }

    #[test]
    fn push_returns_full_on_overflow() {
        let mut t: RulesTable<1> = RulesTable::empty();
        let r = parse_rule_object(
            br#"{"name":"a","family":"crypto","trigger":"x","edge_bps":1,"horizon_ms":1,"max_risk_usd":1}"#,
        )
        .unwrap();
        t.push(r).unwrap();
        assert_eq!(t.push(r), Err(RuleError::Full));
    }

    #[test]
    fn parse_rule_rejects_missing_field() {
        let buf = br#"{"name":"x","family":"crypto"}"#;
        assert_eq!(parse_rule_object(buf), Err(RuleError::MissingField));
    }

    #[test]
    fn parse_rule_rejects_oversized_name() {
        let big = "x".repeat(KEY_LEN + 1);
        let json = format!(
            r#"{{"name":"{big}","family":"crypto","trigger":"x","edge_bps":1,"horizon_ms":1,"max_risk_usd":1}}"#
        );
        assert_eq!(
            parse_rule_object(json.as_bytes()),
            Err(RuleError::NameTooLong)
        );
    }

    #[test]
    fn split_top_level_objects_walks_array() {
        let body = br#"{"a":1},{"b":2}, {"c":3}"#;
        let objs: Vec<_> = split_top_level_objects(body).collect();
        assert_eq!(objs.len(), 3);
        assert_eq!(objs[0], &b"{\"a\":1}"[..]);
        assert_eq!(objs[1], &b"{\"b\":2}"[..]);
        assert_eq!(objs[2], &b"{\"c\":3}"[..]);
    }

    #[test]
    fn split_handles_nested_braces_in_strings() {
        let body = br#"{"x":"a{b}c"}"#;
        let objs: Vec<_> = split_top_level_objects(body).collect();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0], &b"{\"x\":\"a{b}c\"}"[..]);
    }

    #[test]
    fn load_json_round_trips() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ra_rules_{}.json", std::process::id()));
        {
            let mut f = std::fs::File::create(&p).unwrap();
            write!(
                f,
                r#"[
                    {{"name":"r1","family":"crypto","trigger":"x","edge_bps":10,"horizon_ms":1000,"max_risk_usd":50}},
                    {{"name":"r2","family":"politics","trigger":"y","edge_bps":5,"horizon_ms":500,"max_risk_usd":25}}
                ]"#
            )
            .unwrap();
        }
        let (table, skipped) = RulesTable::<4>::load_json(&p).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(skipped, 0);
        assert_eq!(table.lookup(b"r1").unwrap().edge_bps, 10);
        assert_eq!(table.lookup(b"r2").unwrap().family, Family::Politics);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_json_errors_on_missing_array() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ra_rules_bad_{}.json", std::process::id()));
        {
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(b"not an array").unwrap();
        }
        let err = RulesTable::<4>::load_json(&p).unwrap_err();
        assert!(err.to_string().contains("'['"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_json_skips_malformed_entries_but_loads_others() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ra_rules_mixed_{}.json", std::process::id()));
        {
            let mut f = std::fs::File::create(&p).unwrap();
            write!(
                f,
                r#"[
                    {{"name":"r1","family":"crypto","trigger":"x","edge_bps":10,"horizon_ms":1000,"max_risk_usd":50}},
                    {{"name":"bad"}},
                    {{"name":"r3","family":"sports","trigger":"x","edge_bps":3,"horizon_ms":100,"max_risk_usd":10}}
                ]"#
            )
            .unwrap();
        }
        let (table, skipped) = RulesTable::<4>::load_json(&p).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(skipped, 1);
        let _ = std::fs::remove_file(&p);
    }
}
