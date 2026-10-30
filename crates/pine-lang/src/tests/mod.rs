// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! In-crate unit tests of private items (§7.3).

mod lex_layout;
mod lex_literals;
mod policy_alternatives;
mod recovery_internal;

use pine_ir::{Dialect, FileId, Sym, SymTable};

use crate::diag::Diags;
use crate::lex::token::TokKind;
use crate::lex::{lex, Lexed};
use crate::policy::{SyntaxPolicy, POLICY};

/// Lex `src` in `dialect` under `pol`: the stream, its diagnostic codes, and the table.
fn lex_with(src: &str, dialect: Dialect, pol: &SyntaxPolicy) -> (Lexed, Vec<u16>, SymTable) {
    let mut syms = SymTable::new();
    let mut diags = Diags::new();
    let lexed = lex(src, FileId::ROOT, dialect, &mut syms, pol, &mut diags);
    let codes = diags.finish().iter().map(|d| d.code.0).collect();
    (lexed, codes, syms)
}

/// The token kinds of `src`, space-separated, and its codes, under `POLICY`.
fn kinds(src: &str, dialect: Dialect) -> (String, Vec<u16>) {
    let (lexed, codes, _) = lex_with(src, dialect, &POLICY);
    (show(&lexed), codes)
}

/// The token kinds of a stream, space-separated.
fn show(lexed: &Lexed) -> String {
    let names: Vec<String> = lexed.toks.iter().map(|t| format!("{:?}", t.kind)).collect();
    names.join(" ")
}

/// The resolved value of the first `Str` token, if any.
fn first_str(lexed: &Lexed, syms: &SymTable) -> Option<String> {
    lexed
        .toks
        .iter()
        .find(|t| t.kind == TokKind::Str)
        .map(|t| syms.resolve(t.sym).to_string())
}

/// The resolved symbol of the `k`-th token.
fn sym_of(lexed: &Lexed, syms: &SymTable, k: usize) -> String {
    let s: Sym = lexed.toks[k].sym;
    syms.resolve(s).to_string()
}
