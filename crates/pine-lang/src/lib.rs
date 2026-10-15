// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pine-lang` — the Pine Script v5 + v6 front end (impl-01): the `//@version=` gate, the lexer,
//! the INDENT/DEDENT layout machine and the parser into an arena [`Ast`].
//!
//! One entry point, [`parse`]; every identifier, name, string value and annotation text is
//! interned into the caller's [`pine_ir::SymTable`]. Contract types are imported from `pine_ir`,
//! never re-exported.
#![forbid(unsafe_code)]
#![deny(missing_docs, unused_must_use, unreachable_pub)]

use pine_ir::{Diag, FileId, SymTable};

mod ast;
pub mod code;
mod diag;
mod lex;
mod parse;
mod policy;
mod pos;
mod print;
mod version;

#[cfg(test)]
mod tests;

pub use ast::{
    AnnotKind, Annotation, AssignOp, Ast, AstDefect, BinOp, Children, DeclMode, Defect, Node,
    NodeKind, StrForm, UnOp, Visit, Walk,
};
pub use lex::layout::INDENT_UNIT_SPACES;
pub use pos::line_col;
pub use print::{print, structurally_eq};
pub use version::{version_of, VersionProbe};

/// Deepest subtree `parse` accepts (the root's height); deeper is `code::NESTING_TOO_DEEP`.
/// Consumers may recurse over an `Ast` without further bounds (I7).
pub const MAX_DEPTH: u32 = 512;
/// Most diagnostics one `parse` returns; the last is `code::TOO_MANY_ERRORS` when truncated.
pub const MAX_DIAGS: usize = 256;
/// Largest source `parse` accepts, in bytes; larger is `code::SOURCE_TOO_LARGE`.
pub const MAX_SOURCE_BYTES: usize = 1 << 24;
/// Most simultaneously open indented blocks; deeper is `code::BLOCK_NESTING_TOO_DEEP`.
pub const MAX_BLOCK_NESTING: u32 = 64;

/// Parse one source file (contract §3.8.1). Interns every identifier, name, string value and
/// annotation text into `syms`; every `Span` in the result and in the diagnostics names `file`.
/// `Ok` carries an `Ast` with no `Error` node; `Err` carries every diagnostic, sorted by
/// `(span.lo, code)`, each `Severity::Error`, at most `MAX_DIAGS`. A refused `//@version` returns
/// at once with exactly one diagnostic (§4.3). Never panics, terminates on every input (I13).
pub fn parse(src: &str, file: FileId, syms: &mut SymTable) -> Result<Ast, Vec<Diag>> {
    let (partial, diags) = parse::parse_partial(src, file, syms);
    match partial.ast {
        Some(ast) if diags.is_empty() => {
            debug_assert!(ast.validate().is_ok());
            Ok(ast)
        }
        _ => Err(diags),
    }
}
