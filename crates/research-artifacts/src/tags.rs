//! Tag table — loaded from `topic_tagger.py`'s NDJSON output.

use std::io::{self, BufRead, BufReader};
use std::path::Path;

/// Maximum bytes in a Polymarket asset-id / market key.
pub const KEY_LEN: usize = 64;

/// Topic-family enum. Matches the Python tagger's allowed
/// vocabulary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Family {
    /// Crypto market — BTC, ETH, etc.
    Crypto = 0,
    /// US / international politics.
    Politics = 1,
    /// Sports outcomes.
    Sports = 2,
    /// Macro / monetary policy / rates.
    Macro = 3,
    /// Anything that doesn't fit elsewhere.
    Other = 4,
}

impl Family {
    /// Parse from an ASCII tag string. Unknown → `Other`.
    #[inline]
    pub fn from_bytes(s: &[u8]) -> Self {
        match s {
            b"crypto" => Family::Crypto,
            b"politics" => Family::Politics,
            b"sports" => Family::Sports,
            b"macro" => Family::Macro,
            _ => Family::Other,
        }
    }

    /// Numeric tag for storage.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Impact severity tag.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Impact {
    /// Low impact — usually drop the signal.
    Low = 0,
    /// Medium impact — strategy decides.
    Med = 1,
    /// High impact — escalate.
    High = 2,
}

impl Impact {
    /// Parse from an ASCII tag string. Unknown → `Low`.
    #[inline]
    pub fn from_bytes(s: &[u8]) -> Self {
        match s {
            b"high" => Impact::High,
            b"med" => Impact::Med,
            _ => Impact::Low,
        }
    }

    /// Numeric tag for storage.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// One decoded tag row. Returned from `lookup`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// Model-derived true probability, 1e6 fixed-point (0..=1_000_000).
    pub model_p_1e6: u32,
    /// Family enum.
    pub family: Family,
    /// Impact enum.
    pub impact: Impact,
}

/// Why an `insert` rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArtifactError {
    /// The table is at capacity.
    Full,
    /// `key` exceeds `KEY_LEN`.
    KeyTooLong,
    /// Same key already present.
    Duplicate,
}

/// Preallocated table of `(key, p_1e6, family, impact)` rows.
/// `N` is the fixed slot count.
pub struct ArtifactTable<const N: usize> {
    keys: [[u8; KEY_LEN]; N],
    key_lens: [u8; N],
    model_p_1e6: [u32; N],
    family: [u8; N],
    impact: [u8; N],
    count: u32,
}

impl<const N: usize> ArtifactTable<N> {
    /// Build an empty table.
    pub fn empty() -> Self {
        Self {
            keys: [[0u8; KEY_LEN]; N],
            key_lens: [0u8; N],
            model_p_1e6: [0u32; N],
            family: [Family::Other.as_u8(); N],
            impact: [Impact::Low.as_u8(); N],
            count: 0,
        }
    }

    /// Insert one row. Boot-time only.
    pub fn insert(
        &mut self,
        key: &[u8],
        model_p_1e6: u32,
        family: Family,
        impact: Impact,
    ) -> Result<(), ArtifactError> {
        if key.len() > KEY_LEN {
            return Err(ArtifactError::KeyTooLong);
        }
        // Duplicate check.
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            let len = self.key_lens[i] as usize;
            if &self.keys[i][..len] == key {
                return Err(ArtifactError::Duplicate);
            }
            i += 1;
        }
        if n >= N {
            return Err(ArtifactError::Full);
        }
        self.keys[n][..key.len()].copy_from_slice(key);
        self.key_lens[n] = key.len() as u8;
        self.model_p_1e6[n] = model_p_1e6.min(1_000_000);
        self.family[n] = family.as_u8();
        self.impact[n] = impact.as_u8();
        self.count = self.count.wrapping_add(1);
        Ok(())
    }

    /// Look up `key`. Returns `None` if not present.
    /// Zero-alloc; linear scan over `count` slots.
    #[inline]
    pub fn lookup(&self, key: &[u8]) -> Option<Tag> {
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            let len = self.key_lens[i] as usize;
            if &self.keys[i][..len] == key {
                return Some(Tag {
                    model_p_1e6: self.model_p_1e6[i],
                    family: match self.family[i] {
                        0 => Family::Crypto,
                        1 => Family::Politics,
                        2 => Family::Sports,
                        3 => Family::Macro,
                        _ => Family::Other,
                    },
                    impact: match self.impact[i] {
                        2 => Impact::High,
                        1 => Impact::Med,
                        _ => Impact::Low,
                    },
                });
            }
            i += 1;
        }
        None
    }

    /// Number of populated rows.
    #[inline]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the table has no rows.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Boot-time loader. Reads NDJSON from `path`. Each non-blank,
    /// non-comment line is one tag record. Malformed lines are
    /// skipped with a count returned via the `(loaded, skipped)`
    /// tuple.
    ///
    /// Returns `Err` only on fundamental I/O failures (file
    /// missing, unreadable). Per-line malformed-JSON does NOT
    /// short-circuit the boot.
    pub fn load_ndjson(path: &Path) -> io::Result<(Self, usize)> {
        let f = std::fs::File::open(path)?;
        let reader = BufReader::new(f);
        let mut table: Self = ArtifactTable::empty();
        let mut skipped = 0usize;
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            match parse_ndjson_line(trimmed.as_bytes()) {
                Some((key, model_p_1e6, family, impact)) => {
                    if table.insert(key, model_p_1e6, family, impact).is_err() {
                        skipped += 1;
                    }
                }
                None => {
                    skipped += 1;
                }
            }
        }
        Ok((table, skipped))
    }
}

impl<const N: usize> Default for ArtifactTable<N> {
    fn default() -> Self {
        Self::empty()
    }
}

// -----------------------------------------------------------------
// NDJSON line scanner — hand-rolled, no serde_json.
// -----------------------------------------------------------------

/// Parse one NDJSON line into `(key_bytes, model_p_1e6, family, impact)`.
/// Returns `None` for malformed lines.
///
/// We accept `topic_tagger.py`'s shape:
/// `{"id":"...","family":"...","impact":"...","reason":"..."}`
///
/// `model_p_1e6` is derived from `impact`:
/// * high → 700_000 (0.70)
/// * med  → 500_000 (0.50)
/// * low  → 500_000 (0.50, treated as a neutral prior)
///
/// Future versions will let claude-worker pass `p_1e6` directly.
fn parse_ndjson_line(buf: &[u8]) -> Option<(&[u8], u32, Family, Impact)> {
    let id = find_string_field(buf, b"\"id\"")?;
    let family_str = find_string_field(buf, b"\"family\"")?;
    let impact_str = find_string_field(buf, b"\"impact\"")?;
    let family = Family::from_bytes(family_str);
    let impact = Impact::from_bytes(impact_str);

    // Look for an optional explicit "p_1e6" override.
    let p_1e6 = match find_int_field(buf, b"\"p_1e6\"") {
        Some(v) if (0..=1_000_000).contains(&v) => v as u32,
        _ => derive_p_from_impact(impact),
    };

    Some((id, p_1e6, family, impact))
}

fn derive_p_from_impact(impact: Impact) -> u32 {
    match impact {
        Impact::High => 700_000,
        Impact::Med => 500_000,
        Impact::Low => 500_000,
    }
}

/// Public re-export of `find_string_field` for sibling modules.
pub(crate) fn find_string_field_pub<'a>(buf: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    find_string_field(buf, key)
}

/// Public re-export of `find_int_field` for sibling modules.
pub(crate) fn find_int_field_pub(buf: &[u8], key: &[u8]) -> Option<i64> {
    find_int_field(buf, key)
}

/// Search for `"<key>":"<value>"` and return the byte range of
/// `value`. Handles backslash escapes by skipping the next char.
fn find_string_field<'a>(buf: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let kpos = memchr::memmem::find(buf, key)?;
    let mut i = kpos + key.len();
    while i < buf.len() && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= buf.len() || buf[i] != b':' {
        return None;
    }
    i += 1;
    while i < buf.len() && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= buf.len() || buf[i] != b'"' {
        return None;
    }
    let start = i + 1;
    let mut j = start;
    while j < buf.len() {
        match buf[j] {
            b'\\' => j += 2,
            b'"' => return Some(&buf[start..j]),
            _ => j += 1,
        }
    }
    None
}

/// Search for `"<key>":<int>` (no quotes around value). Returns
/// the integer value or `None` if absent / malformed.
fn find_int_field(buf: &[u8], key: &[u8]) -> Option<i64> {
    let kpos = memchr::memmem::find(buf, key)?;
    let mut i = kpos + key.len();
    while i < buf.len() && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= buf.len() || buf[i] != b':' {
        return None;
    }
    i += 1;
    while i < buf.len() && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    let neg = i < buf.len() && buf[i] == b'-';
    if neg {
        i += 1;
    }
    let start = i;
    while i < buf.len() && buf[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    let mut v: i64 = 0;
    for &b in &buf[start..i] {
        v = v
            .checked_mul(10)?
            .checked_add((b - b'0') as i64)?;
    }
    if neg {
        v = -v;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    type T = ArtifactTable<8>;

    #[test]
    fn insert_then_lookup_round_trips() {
        let mut t: T = ArtifactTable::empty();
        t.insert(b"0xabc", 700_000, Family::Crypto, Impact::High).unwrap();
        let got = t.lookup(b"0xabc").unwrap();
        assert_eq!(got.model_p_1e6, 700_000);
        assert_eq!(got.family, Family::Crypto);
        assert_eq!(got.impact, Impact::High);
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        let t: T = ArtifactTable::empty();
        assert!(t.lookup(b"missing").is_none());
    }

    #[test]
    fn insert_rejects_duplicates() {
        let mut t: T = ArtifactTable::empty();
        t.insert(b"x", 500_000, Family::Other, Impact::Low).unwrap();
        assert_eq!(
            t.insert(b"x", 600_000, Family::Crypto, Impact::High),
            Err(ArtifactError::Duplicate)
        );
    }

    #[test]
    fn insert_rejects_oversized_key() {
        let mut t: T = ArtifactTable::empty();
        let big = [b'x'; KEY_LEN + 1];
        assert_eq!(
            t.insert(&big, 0, Family::Other, Impact::Low),
            Err(ArtifactError::KeyTooLong)
        );
    }

    #[test]
    fn insert_fills_to_capacity() {
        let mut t: ArtifactTable<2> = ArtifactTable::empty();
        t.insert(b"a", 0, Family::Other, Impact::Low).unwrap();
        t.insert(b"b", 0, Family::Other, Impact::Low).unwrap();
        assert_eq!(
            t.insert(b"c", 0, Family::Other, Impact::Low),
            Err(ArtifactError::Full)
        );
    }

    #[test]
    fn parse_ndjson_line_extracts_fields() {
        let line = br#"{"id":"0xabc","family":"crypto","impact":"high","reason":"BTC"}"#;
        let (id, p, f, i) = parse_ndjson_line(line).unwrap();
        assert_eq!(id, b"0xabc");
        assert_eq!(p, 700_000);
        assert_eq!(f, Family::Crypto);
        assert_eq!(i, Impact::High);
    }

    #[test]
    fn parse_ndjson_line_uses_p_1e6_override_when_present() {
        let line = br#"{"id":"x","family":"other","impact":"low","p_1e6":420000}"#;
        let (_id, p, _f, _i) = parse_ndjson_line(line).unwrap();
        assert_eq!(p, 420_000);
    }

    #[test]
    fn parse_ndjson_line_rejects_missing_fields() {
        let line = br#"{"family":"crypto","impact":"high"}"#;
        assert!(parse_ndjson_line(line).is_none());
    }

    #[test]
    fn family_and_impact_parse_unknown_to_other_low() {
        assert_eq!(Family::from_bytes(b"alien"), Family::Other);
        assert_eq!(Impact::from_bytes(b"loud"), Impact::Low);
    }

    #[test]
    fn load_ndjson_from_file() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ra_test_{}.ndjson", std::process::id()));
        {
            let mut f = std::fs::File::create(&p).unwrap();
            writeln!(f, "# comment line skipped").unwrap();
            writeln!(
                f,
                r#"{{"id":"0xabc","family":"crypto","impact":"high","reason":"x"}}"#
            )
            .unwrap();
            writeln!(f, "garbage-line").unwrap();
            writeln!(
                f,
                r#"{{"id":"0xdef","family":"politics","impact":"med","reason":"y"}}"#
            )
            .unwrap();
            writeln!(f).unwrap(); // blank line
        }
        let (table, skipped) = ArtifactTable::<8>::load_ndjson(&p).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(skipped, 1);
        assert_eq!(table.lookup(b"0xabc").unwrap().family, Family::Crypto);
        assert_eq!(table.lookup(b"0xdef").unwrap().impact, Impact::Med);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn find_int_field_parses_negative_and_positive() {
        let buf = br#"{"x":42,"y":-7}"#;
        assert_eq!(find_int_field(buf, b"\"x\""), Some(42));
        assert_eq!(find_int_field(buf, b"\"y\""), Some(-7));
        assert_eq!(find_int_field(buf, b"\"z\""), None);
    }
}
