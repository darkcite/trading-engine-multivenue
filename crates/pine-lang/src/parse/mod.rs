// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Parser state, the token cursor, the line driver and `parse`'s body (§4.1, §4.7, §4.8).
//!

mod decl;
mod expr;
mod stmt;

use pine_ir::{Diag, DiagCode, Dialect, FileId, NodeId, Span, SymTable};

use crate::ast::build::Builder;
use crate::ast::{AnnotKind, Annotation, Ast, NodeKind};
use crate::code;
use crate::diag::Diags;
use crate::lex::token::{SoftKw, TokKind, Token, LINE_START};
use crate::lex::{self, Lexed};
use crate::policy::{SyntaxPolicy, POLICY};
use crate::version::{self, VersionProbe};
use crate::MAX_SOURCE_BYTES;

/// The arena `parse_with_policy` built, `Error` nodes included; `None` when the size or version
/// gate refused the source before any arena existed. Never leaves the crate.
#[derive(Debug)]
pub(crate) struct Partial {
    /// The arena.
    pub(crate) ast: Option<Ast>,
}

/// Steps 0–3 of §4.1 under `POLICY`.
pub(crate) fn parse_partial(src: &str, file: FileId, syms: &mut SymTable) -> (Partial, Vec<Diag>) {
    parse_with_policy(src, file, syms, &POLICY)
}

/// Steps 0–3 of §4.1 under an explicit policy (§4.12).
pub(crate) fn parse_with_policy(
    src: &str,
    file: FileId,
    syms: &mut SymTable,
    pol: &SyntaxPolicy,
) -> (Partial, Vec<Diag>) {
    let mut diags = Diags::new();
    if src.len() > MAX_SOURCE_BYTES {
        diags.source_too_large(file, src.len());
        return (Partial { ast: None }, diags.finish());
    }
    let (dialect, version_span) = match version::version_of_with(src, file, pol) {
        VersionProbe::Supported { dialect, span } => (dialect, span),
        refused => {
            diags.version_refusal(refused);
            return (Partial { ast: None }, diags.finish());
        }
    };
    let Lexed { toks, annos, .. } = lex::lex(src, file, dialect, syms, pol, &mut diags);
    let mut p = Parser {
        src: src.as_bytes(),
        file,
        dialect,
        pol,
        toks,
        pos: 0,
        diags,
        build: Builder::new(file, dialect, src.len()),
        depth: 0,
        recovering: false,
        last_error: NodeId::NONE,
        annos,
        loop_depth: 0,
        if_chain: Vec::new(),
        in_list: 0,
        scope_depth: 0,
    };
    p.script();
    let Parser {
        build,
        diags,
        annos,
        ..
    } = p;
    (
        Partial {
            ast: Some(build.finish(version_span, syms, annos)),
        },
        diags.finish(),
    )
}

/// The parser.
pub(super) struct Parser<'s, 'p> {
    src: &'s [u8],
    file: FileId,
    dialect: Dialect,
    pol: &'p SyntaxPolicy,
    toks: Vec<Token>,
    pos: usize,
    diags: Diags,
    build: Builder,
    /// Expression recursion depth (bounded by `MAX_DEPTH`, §4.9).
    depth: u32,
    /// Rule 2: set by a recovery, cleared at the next statement start.
    recovering: bool,
    last_error: NodeId,
    annos: Vec<Annotation>,
    /// Open lists (arguments, tuples, parameters, type arguments) of the current statement:
    /// their `,` is in the synchronisation set.
    in_list: u32,
    /// The links `(lo, cond, then)` of the `else if` chains being parsed, innermost last.
    if_chain: Vec<(u32, NodeId, NodeId)>,
    /// Enclosing loop bodies (2010); reset inside a function body.
    loop_depth: u32,
    /// Enclosing blocks and bodies: 0 is the global scope (2027).
    scope_depth: u32,
}

impl Parser<'_, '_> {
    // ---- the token cursor -------------------------------------------------------------------

    /// The current token (`Eof` once the stream is exhausted).
    fn peek(&self) -> Token {
        self.toks[self.pos.min(self.toks.len() - 1)]
    }

    /// The token `k` ahead of the current one.
    fn peek_at(&self, k: usize) -> Token {
        self.toks[(self.pos + k).min(self.toks.len() - 1)]
    }

    /// Consume the current token (never past `Eof`).
    fn bump(&mut self) -> Token {
        let t = self.peek();
        if t.kind != TokKind::Eof {
            self.pos += 1;
        }
        t
    }

    /// The span of a token.
    fn tspan(&self, t: Token) -> Span {
        Span::new(self.file, t.lo, t.hi)
    }

    /// `hi` of the last consumed token.
    fn prev_hi(&self) -> u32 {
        if self.pos == 0 {
            0
        } else {
            self.toks[self.pos - 1].hi
        }
    }

    // ---- recovery (the T9 rules in their first form) -----------------------------------------

    /// §4.8 `recover`: report `c` at `at` once (rule 3; never at an `Error` token, which the
    /// lexer reported), unless a recovery is already running (rule 2: return its node); skip to
    /// the synchronisation set — `Eof`, `Newline`/`Dedent` at depth 0, a statement keyword on a
    /// `LINE_START` token, the enclosing list's `,` or closer — consuming at least one token
    /// unless at `Dedent`/`Eof` (rule 1); return the recovery's one `Error` node.
    fn recover(&mut self, c: DiagCode, at: Span, msg: &'static str) -> NodeId {
        if self.recovering {
            return self.last_error;
        }
        let t = self.peek();
        if !(t.kind == TokKind::Error && t.lo == at.lo) {
            self.diags.at(c, self.file, at.lo, at.hi, msg);
        }
        let start = self.pos;
        // At a statement boundary (a node built after its block's `Dedent`, e.g. a too-deep `If`
        // chain) the statement is already consumed: skip nothing.
        if self.pos > 0 && self.toks[self.pos - 1].kind == TokKind::Dedent {
            self.recovering = true;
            self.last_error = self.build.leaf(NodeKind::Error, 0, 0, 0, at);
            return self.last_error;
        }
        let mut depth = 0u32;
        loop {
            let t = self.peek();
            match t.kind {
                TokKind::Eof => break,
                TokKind::Newline | TokKind::Dedent | TokKind::Indent if depth == 0 => break,
                TokKind::Comma if depth == 0 && self.in_list > 0 => break,
                TokKind::LParen | TokKind::LBracket => {
                    depth += 1;
                    self.bump();
                }
                TokKind::RParen | TokKind::RBracket => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    self.bump();
                }
                _ if depth == 0 && t.flags & LINE_START != 0 && is_sync_keyword(t) => break,
                _ => {
                    self.bump();
                }
            }
        }
        if self.pos == start && !matches!(self.peek().kind, TokKind::Dedent | TokKind::Eof) {
            self.bump();
        }
        self.recovering = true;
        let span = Span::new(self.file, at.lo, at.hi.max(self.prev_hi()));
        self.last_error = self.build.leaf(NodeKind::Error, 0, 0, 0, span);
        self.last_error
    }

    /// A diagnostic sited at a token that needs no skipping (2006, 2007): reported once, with
    /// its one `Error` node outside the tree.
    fn report_node(
        &mut self,
        c: DiagCode,
        at: Span,
        msg: &'static str,
        note: Span,
        nmsg: &'static str,
    ) {
        self.diags.at_with_note(c, at, msg, note, nmsg);
        self.build.leaf(NodeKind::Error, 0, 0, 0, at);
    }

    /// A node over built children; too deep is 2026 and an `Error` node.
    fn alloc(&mut self, kind: NodeKind, flags: u8, slots: [u32; 4], span: Span) -> NodeId {
        match self.build.node(kind, flags, slots, span) {
            Ok(id) => id,
            Err(_) => self.recover(
                code::NESTING_TOO_DEEP,
                span,
                "an expression nested too deeply",
            ),
        }
    }

    /// Skip the rest of the statement: to its line's `Newline` (not consumed) or `Eof`, and
    /// through any block the line opens.
    fn skip_statement(&mut self) {
        let mut depth = 0u32;
        loop {
            match self.peek().kind {
                TokKind::Eof => return,
                TokKind::Newline if depth == 0 => {
                    if self.peek_at(1).kind != TokKind::Indent {
                        return;
                    }
                    self.bump();
                }
                TokKind::Indent => {
                    depth += 1;
                    self.bump();
                }
                TokKind::Dedent => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// The doc run ending on the line above the statement starting at `lo` (§3.7, §4.9): the
    /// maximal run of lines holding one non-script-level annotation each, directly above.
    /// Computed before the statement is parsed, so its block's own runs cannot interfere.
    fn doc_run(&self, lo: u32) -> Option<(u32, u32)> {
        let end = self.annos.partition_point(|a| a.span.hi <= lo);
        let mut k = end;
        let mut below = lo;
        while k > 0 {
            let a = self.annos[k - 1];
            let script_level = matches!(
                a.kind,
                AnnotKind::Version | AnnotKind::Description | AnnotKind::StrategyAlertMessage
            );
            if script_level
                || !one_line_break_between(self.src, a.span.hi, below)
                || !alone_on_its_line(self.src, a.span.lo)
            {
                break;
            }
            below = a.span.lo;
            k -= 1;
        }
        if k < end {
            Some((k as u32, (end - k) as u32))
        } else {
            None
        }
    }
}

/// `src[hi..lo]` is horizontal whitespace around exactly one line terminator.
fn one_line_break_between(src: &[u8], hi: u32, lo: u32) -> bool {
    let mut i = hi as usize;
    let end = lo as usize;
    let mut breaks = 0u32;
    while i < end {
        match src[i] {
            b' ' | b'\t' => {}
            b'\r' => {
                breaks += 1;
                if i + 1 < end && src[i + 1] == b'\n' {
                    i += 1;
                }
            }
            b'\n' => breaks += 1,
            _ => return false,
        }
        i += 1;
    }
    breaks == 1
}

/// Only spaces/tabs precede byte `lo` on its line.
fn alone_on_its_line(src: &[u8], lo: u32) -> bool {
    let mut i = lo as usize;
    while i > 0 {
        match src[i - 1] {
            b' ' | b'\t' => i -= 1,
            b'\n' | b'\r' => return true,
            _ => return false,
        }
    }
    true
}

/// A statement keyword (§4.8 statement-level synchronisation set).
fn is_sync_keyword(t: Token) -> bool {
    match t.kind {
        TokKind::If
        | TokKind::For
        | TokKind::While
        | TokKind::Switch
        | TokKind::Var
        | TokKind::Varip
        | TokKind::Break
        | TokKind::Continue => true,
        TokKind::Ident => matches!(
            t.kw,
            SoftKw::Import
                | SoftKw::Export
                | SoftKw::Type
                | SoftKw::Enum
                | SoftKw::Method
                | SoftKw::Once
        ),
        _ => false,
    }
}
