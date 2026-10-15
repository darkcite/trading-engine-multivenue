// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The diagnostic sink and every message builder — the only module of the crate that constructs a
//! `String` (§2, §4.13).
//!
//! COPY-DOCTRINE: error path at load only — a Diag's message and notes are rendered once per
//! diagnostic; never reached by the engine loop.

use pine_ir::{Diag, DiagCode, FileId, Severity, Span};

use crate::code;
use crate::version::VersionProbe;
use crate::MAX_DIAGS;

/// The sink: rule 3 (one diagnostic per source position), the `MAX_DIAGS` budget and the final
/// `(span.lo, code)` sort (§4.8, I17).
#[derive(Debug)]
pub(crate) struct Diags {
    list: Vec<Diag>,
    /// Set when the budget ran out; the 2025 diagnostic is held here, appended last by `finish`.
    truncated: Option<Diag>,
}

impl Diags {
    /// An empty sink.
    pub(crate) fn new() -> Self {
        Self {
            list: Vec::new(),
            truncated: None,
        }
    }

    /// The budget ran out: the parser stops at the next statement boundary (§4.8).
    pub(crate) fn is_full(&self) -> bool {
        self.truncated.is_some()
    }

    /// Keep `d` unless an earlier diagnostic starts at the same byte (rule 3) or the budget is
    /// spent; the `MAX_DIAGS`-th is replaced by `code::TOO_MANY_ERRORS`.
    fn push(&mut self, d: Diag) {
        if self.truncated.is_some() {
            return;
        }
        let mut i = 0usize;
        while i < self.list.len() {
            if self.list[i].span.lo == d.span.lo {
                return;
            }
            i += 1;
        }
        if self.list.len() + 1 >= MAX_DIAGS {
            self.truncated = Some(error(
                code::TOO_MANY_ERRORS,
                d.span,
                "too many errors: the diagnostic budget is spent and parsing stopped".to_string(),
            ));
            return;
        }
        self.list.push(d);
    }

    /// The `Err` vector: sorted by `(span.lo, code)`, ending with 2025 exactly when truncated.
    pub(crate) fn finish(mut self) -> Vec<Diag> {
        self.list.sort_by_key(|d| (d.span.lo, d.code.0));
        if let Some(t) = self.truncated.take() {
            self.list.push(t);
        }
        self.list
    }

    // ---- message builders -------------------------------------------------------------------

    /// 1015.
    pub(crate) fn source_too_large(&mut self, file: FileId, len: usize) {
        let msg = format!(
            "source is {len} bytes; the largest source accepted is {} bytes",
            crate::MAX_SOURCE_BYTES
        );
        self.push(error(code::SOURCE_TOO_LARGE, Span::new(file, 0, 0), msg));
    }

    /// The version gate's one diagnostic (3001, 2101, 2103 or 2105).
    pub(crate) fn version_refusal(&mut self, probe: VersionProbe) {
        let d = match probe {
            VersionProbe::Supported { .. } => return,
            VersionProbe::TooOld { version, span } => error(
                code::DIALECT_TOO_OLD,
                span,
                format!("`//@version={version}` is not supported; expected `//@version=5` or `=6`"),
            ),
            VersionProbe::Missing => error(
                code::DIALECT_TOO_OLD,
                Span::default(),
                "no `//@version=` annotation (version 1 is assumed); expected `//@version=5` or \
                 `=6`"
                    .to_string(),
            ),
            VersionProbe::Unknown { version, span } => error(
                code::VERSION_UNKNOWN,
                span,
                format!("`//@version={version}` names no known Pine version; expected 5 or 6"),
            ),
            VersionProbe::Malformed { span } => error(
                code::VERSION_MALFORMED,
                span,
                "malformed version annotation; expected `//@version=` followed by digits"
                    .to_string(),
            ),
            VersionProbe::Conflict { first, second } => {
                let mut d = error(
                    code::VERSION_CONFLICT,
                    second,
                    "a second `//@version=` annotation names a different version".to_string(),
                );
                d.notes.push((first, "the first annotation".to_string()));
                d
            }
        };
        self.push(d);
    }

    /// A lexical or layout diagnostic at `[lo, hi)` with a fixed sentence.
    pub(crate) fn at(&mut self, c: DiagCode, file: FileId, lo: u32, hi: u32, msg: &'static str) {
        self.push(error(c, Span::new(file, lo, hi), msg.to_string()));
    }

    /// A diagnostic with one related location.
    pub(crate) fn at_with_note(
        &mut self,
        c: DiagCode,
        span: Span,
        msg: &'static str,
        note: Span,
        note_msg: &'static str,
    ) {
        let mut d = error(c, span, msg.to_string());
        d.notes.push((note, note_msg.to_string()));
        self.push(d);
    }
}

/// An error-severity diagnostic without notes.
fn error(code: DiagCode, span: Span, msg: String) -> Diag {
    Diag {
        code,
        sev: Severity::Error,
        span,
        msg,
        notes: Vec::new(),
    }
}
