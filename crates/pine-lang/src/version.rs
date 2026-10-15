// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The `//@version=` gate (§3.8, §4.3): `version_of`, `VersionProbe` and the comment scanner the
//! lexer shares.

use pine_ir::{DiagCode, Dialect, FileId, Span};

use crate::policy::{SyntaxPolicy, POLICY};

/// What the `//@version=` scan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionProbe {
    /// `//@version=5` or `=6`.
    Supported {
        /// The dialect.
        dialect: Dialect,
        /// The annotation comment.
        span: Span,
    },
    /// `//@version=N`, N in 1..=4.
    TooOld {
        /// N.
        version: u32,
        /// The annotation comment.
        span: Span,
    },
    /// `//@version=N`, N = 0 or N >= 7.
    Unknown {
        /// N, saturated at `u32::MAX`.
        version: u32,
        /// The annotation comment.
        span: Span,
    },
    /// No annotation: TradingView assumes version 1.
    Missing,
    /// `//@version` not followed by `=` and decimal digits.
    Malformed {
        /// The annotation comment.
        span: Span,
    },
    /// Two annotations with different numbers.
    Conflict {
        /// The first.
        first: Span,
        /// The first that disagrees with it.
        second: Span,
    },
}

impl VersionProbe {
    /// The dialect, when supported.
    #[must_use]
    pub const fn dialect(self) -> Option<Dialect> {
        match self {
            Self::Supported { dialect, .. } => Some(dialect),
            _ => None,
        }
    }

    /// The version TradingView runs the script as: N when declared, 1 when `Missing`.
    #[must_use]
    pub const fn declared(self) -> Option<u32> {
        match self {
            Self::Supported { dialect, .. } => Some(dialect as u32),
            Self::TooOld { version, .. } | Self::Unknown { version, .. } => Some(version),
            Self::Missing => Some(1),
            Self::Malformed { .. } | Self::Conflict { .. } => None,
        }
    }

    /// The refusal `parse` returns, or `None` for a supported dialect.
    #[must_use]
    pub const fn refusal(self) -> Option<DiagCode> {
        match self {
            Self::Supported { .. } => None,
            Self::TooOld { .. } | Self::Missing => Some(crate::code::DIALECT_TOO_OLD),
            Self::Unknown { .. } => Some(crate::code::VERSION_UNKNOWN),
            Self::Malformed { .. } => Some(crate::code::VERSION_MALFORMED),
            Self::Conflict { .. } => Some(crate::code::VERSION_CONFLICT),
        }
    }
}

/// Find the `//@version=` annotation without lexing the body (§3.8, §4.3): `parse`'s first step
/// and a public pre-scan. Strings are skipped (both quote styles, single-line and multiline, in
/// both dialects); every `//` comment is inspected with the lexer's own comment scanner (I11).
#[must_use]
pub fn version_of(src: &str, file: FileId) -> VersionProbe {
    version_of_with(src, file, &POLICY)
}

/// `version_of` under an explicit policy (pin:01-version-form, pin:01-version-conflict).
pub(crate) fn version_of_with(src: &str, file: FileId, pol: &SyntaxPolicy) -> VersionProbe {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    let mut first: Option<(u32, Span)> = None;
    let mut conflict: Option<Span> = None;
    let mut malformed: Option<Span> = None;
    while i < n {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            i = skip_string(b, i, pol.string_line_wrap);
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'/' {
            let end = scan_comment(b, i);
            if let Some(rest) = version_key_rest(&b[i + 2..end]) {
                let span = Span::new(file, i as u32, end as u32);
                match pragma_number(rest, pol.version_form_strict) {
                    Some(v) => match first {
                        None => first = Some((v, span)),
                        Some((f, _)) => {
                            if f != v && conflict.is_none() && pol.version_conflict_refused {
                                conflict = Some(span);
                            }
                        }
                    },
                    None => {
                        if malformed.is_none() {
                            malformed = Some(span);
                        }
                    }
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    if let Some(span) = malformed {
        return VersionProbe::Malformed { span };
    }
    match (first, conflict) {
        (None, _) => VersionProbe::Missing,
        (Some((_, f)), Some(s)) => VersionProbe::Conflict {
            first: f,
            second: s,
        },
        (Some((v, span)), None) => match Dialect::from_version(v) {
            Some(dialect) => VersionProbe::Supported { dialect, span },
            None if (1..=4).contains(&v) => VersionProbe::TooOld { version: v, span },
            None => VersionProbe::Unknown { version: v, span },
        },
    }
}

/// The end of the `//` comment starting at `i`: the index of its line terminator, or `b.len()`.
/// Shared by the pre-scan and the lexer, so they agree on what is a comment (I11).
pub(crate) fn scan_comment(b: &[u8], i: usize) -> usize {
    let n = b.len();
    let mut end = i + 2;
    while end < n && b[end] != b'\n' && b[end] != b'\r' {
        end += 1;
    }
    end
}

/// For a comment text (after `//`) that is the `@version` annotation, the bytes after the key:
/// the key must be followed by the end of the comment, whitespace or `=` (§3.7); anything else
/// (`//@versions`) is a plain comment.
fn version_key_rest(text: &[u8]) -> Option<&[u8]> {
    const KEY: &[u8] = b"@version";
    if text.len() < KEY.len() || &text[..KEY.len()] != KEY {
        return None;
    }
    let rest = &text[KEY.len()..];
    match rest.first() {
        None | Some(b' ') | Some(b'\t') | Some(b'=') => Some(rest),
        _ => None,
    }
}

/// `=` then decimal digits then only spaces/tabs; with `strict == false` spaces/tabs may also
/// surround the `=` (pin:01-version-form). The value saturates at `u32::MAX`.
fn pragma_number(t: &[u8], strict: bool) -> Option<u32> {
    let mut i = 0usize;
    if !strict {
        i = skip_blanks(t, i);
    }
    if t.get(i) != Some(&b'=') {
        return None;
    }
    i += 1;
    if !strict {
        i = skip_blanks(t, i);
    }
    let mut v: u32 = 0;
    let mut digits = 0u32;
    while i < t.len() && t[i].is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add(u32::from(t[i] - b'0'));
        digits += 1;
        i += 1;
    }
    if digits == 0 || skip_blanks(t, i) != t.len() {
        return None;
    }
    Some(v)
}

/// The index of the first byte at or after `i` that is not a space or tab.
fn skip_blanks(t: &[u8], mut i: usize) -> usize {
    while i < t.len() && (t[i] == b' ' || t[i] == b'\t') {
        i += 1;
    }
    i
}

/// Skip one string literal starting at `i` (a quote); returns the index after it. A single-line
/// literal stops at its line terminator (1001 in the lexer, which ends the token there too) unless
/// pin:01-string-line-wrap is relaxed; a
/// multiline one runs to its closing triple or EOF. Escapes are honoured in both.
fn skip_string(b: &[u8], i: usize, line_wrap: bool) -> usize {
    let q = b[i];
    let n = b.len();
    if i + 2 < n && b[i + 1] == q && b[i + 2] == q {
        let mut j = i + 3;
        while j < n {
            if b[j] == b'\\' {
                j += 2;
                continue;
            }
            if b[j] == q && j + 2 < n && b[j + 1] == q && b[j + 2] == q {
                return j + 3;
            }
            j += 1;
        }
        return n;
    }
    let mut j = i + 1;
    while j < n {
        match b[j] {
            b'\\' => {
                if j + 1 < n && (b[j + 1] == b'\n' || b[j + 1] == b'\r') && !line_wrap {
                    return j + 1;
                }
                j += 2;
            }
            b'\n' | b'\r' if !line_wrap => return j,
            x if x == q => return j + 1,
            _ => j += 1,
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_table() {
        let f = FileId::ROOT;
        assert!(matches!(
            version_of("//@version=6\nx = 1", f),
            VersionProbe::Supported {
                dialect: Dialect::V6,
                ..
            }
        ));
        assert!(matches!(
            version_of("//@version=5", f),
            VersionProbe::Supported {
                dialect: Dialect::V5,
                ..
            }
        ));
        assert!(matches!(
            version_of("//@version=4", f),
            VersionProbe::TooOld { version: 4, .. }
        ));
        assert!(matches!(
            version_of("//@version=7", f),
            VersionProbe::Unknown { version: 7, .. }
        ));
        assert!(matches!(
            version_of("//@version=0", f),
            VersionProbe::Unknown { version: 0, .. }
        ));
        assert_eq!(version_of("x = 1", f), VersionProbe::Missing);
        assert!(matches!(
            version_of("//@version=", f),
            VersionProbe::Malformed { .. }
        ));
        assert!(matches!(
            version_of("//@version = 6", f),
            VersionProbe::Malformed { .. }
        ));
        assert!(matches!(
            version_of("//@version=6x", f),
            VersionProbe::Malformed { .. }
        ));
        assert!(matches!(
            version_of("//@version=6  \t", f),
            VersionProbe::Supported { .. }
        ));
        assert!(matches!(
            version_of("//@version=5\n//@version=6", f),
            VersionProbe::Conflict { .. }
        ));
        assert!(matches!(
            version_of("//@version=6\n//@version=6", f),
            VersionProbe::Supported { .. }
        ));
        assert_eq!(version_of("s = \"//@version=4\"", f), VersionProbe::Missing);
        assert_eq!(
            version_of("s = \"\"\"\n//@version=4\n\"\"\"", f),
            VersionProbe::Missing
        );
        assert!(matches!(
            version_of("x = 1\nindicator(\"a\")\n//@version=6", f),
            VersionProbe::Supported { .. }
        ));
        assert!(matches!(
            version_of("x = 1 //@version=6", f),
            VersionProbe::Supported { .. }
        ));
        assert_eq!(VersionProbe::Missing.refusal(), Some(DiagCode(3001)));
        assert_eq!(VersionProbe::Missing.declared(), Some(1));
    }
}
