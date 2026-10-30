// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Compiler-annotation comments → `Annotation` rows (§3.7).

use pine_ir::{FileId, Span, SymTable};

use crate::ast::{AnnotKind, Annotation};

/// The ten documented keys, by kind.
const KEYS: [(&[u8], AnnotKind); 10] = [
    (b"version", AnnotKind::Version),
    (b"description", AnnotKind::Description),
    (b"function", AnnotKind::Function),
    (b"param", AnnotKind::Param),
    (b"returns", AnnotKind::Returns),
    (b"type", AnnotKind::Type),
    (b"enum", AnnotKind::Enum),
    (b"field", AnnotKind::Field),
    (b"variable", AnnotKind::Variable),
    (b"strategy_alert_message", AnnotKind::StrategyAlertMessage),
];

/// The annotation row of the comment `src[lo..end]` (`//` through the last byte before the line
/// terminator), or `None` for a plain comment. The key must be followed by whitespace, the end of
/// the comment, or (for `version` only) `=`; the text after it has leading spaces/tabs stripped
/// and trailing whitespace trimmed, and is interned from the borrowed slice.
pub(crate) fn annotation(
    src: &str,
    lo: usize,
    end: usize,
    file: FileId,
    syms: &mut SymTable,
) -> Option<Annotation> {
    let b = src.as_bytes();
    let t = lo + 2;
    if t >= end || b[t] != b'@' {
        return None;
    }
    let mut k = 0usize;
    while k < KEYS.len() {
        let (key, kind) = KEYS[k];
        let ke = t + 1 + key.len();
        if ke <= end && &b[t + 1..ke] == key {
            let next = if ke < end { b[ke] } else { b' ' };
            let mut a = ke;
            let ok = match next {
                b' ' | b'\t' => true,
                b'=' if kind == AnnotKind::Version => {
                    a += 1;
                    true
                }
                _ => next.is_ascii_whitespace(),
            };
            if !ok {
                return None;
            }
            while a < end && (b[a] == b' ' || b[a] == b'\t') {
                a += 1;
            }
            let mut z = end;
            while z > a && b[z - 1].is_ascii_whitespace() {
                z -= 1;
            }
            return Some(Annotation {
                kind,
                _pad: [0; 3],
                text: syms.intern(&src[a..z]),
                span: Span::new(file, lo as u32, end as u32),
            });
        }
        k += 1;
    }
    None
}
