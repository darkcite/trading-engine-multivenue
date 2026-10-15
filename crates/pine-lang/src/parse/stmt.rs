// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Statements (§4.7): the line/block driver, statement dispatch, the bounded lookaheads,
//! declarations, assignments, the structures and their bodies.

use pine_ir::{NodeId, Qual, Span};

use super::Parser;
use crate::ast::{AssignOp, DeclMode, NodeKind};
use crate::code;
use crate::lex::token::{SoftKw, TokKind, LINE_START};

/// Most tokens the typed-declaration scan reads.
const TYPED_DECL_SCAN: usize = 64;

impl Parser<'_, '_> {
    /// `script = { line } Eof`.
    pub(super) fn script(&mut self) {
        let mark = self.build.mark();
        self.lines(false);
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, 0, self.src.len() as u32);
        if self
            .build
            .node(NodeKind::Script, 0, [start, len, 0, 0], span)
            .is_err()
        {
            self.diags.at(
                code::NESTING_TOO_DEEP,
                self.file,
                0,
                0,
                "the script is nested too deeply",
            );
            // The root must exist: an empty one stands in (the parse is refused anyway).
            self.build.leaf(NodeKind::Script, 0, start, 0, span);
        }
    }

    /// Lines until `Eof` (or, inside a block, its `Dedent`, consumed); each statement is pushed
    /// as an item of the open list.
    fn lines(&mut self, in_block: bool) {
        loop {
            if self.diags.is_full() {
                // The budget is spent: stop at this statement boundary (§4.8).
                self.pos = self.toks.len() - 1;
                return;
            }
            match self.peek().kind {
                TokKind::Eof => return,
                TokKind::Dedent => {
                    self.bump();
                    if in_block {
                        return;
                    }
                }
                TokKind::Newline => {
                    self.bump();
                }
                TokKind::Indent => self.unexpected_indent(),
                _ => self.line(),
            }
        }
    }

    /// An `Indent` where a statement is expected: 1008 once; the indented run is parsed as a
    /// block and wrapped in one `Error` node (§4.8 layout errors).
    fn unexpected_indent(&mut self) {
        let t = self.bump();
        let at = self.tspan(t);
        // An operand missing before this indent was already reported here as 1008 (rule 3):
        // its `Error` node stands for the run.
        let reuse = self.last_error != NodeId::NONE && self.build.span(self.last_error).lo == at.lo;
        let e = if reuse {
            self.last_error
        } else {
            self.diags.at(
                code::UNEXPECTED_INDENT,
                self.file,
                at.lo,
                at.hi,
                "unexpected indentation",
            );
            self.build.leaf(NodeKind::Error, 0, 0, 0, at)
        };
        self.recovering = false;
        let mark = self.build.mark();
        self.lines(true);
        self.build.close_list(mark);
        self.build.item(e);
    }

    /// `line = stmt { "," stmt } line_end`.
    fn line(&mut self) {
        let from = self.pos;
        loop {
            self.recovering = false;
            self.in_list = 0;
            let first = self.peek();
            let s = self.stmt();
            self.attach_doc(s, first.lo);
            self.build.item(s);
            if self.recovering
                && self.peek().kind == TokKind::Newline
                && self.peek_at(1).kind == TokKind::Indent
            {
                // A header that failed still owns its block (§4.8): parse it, keep nothing.
                self.block();
            }
            if self.peek().kind != TokKind::Comma || self.closed_block_since(from) {
                break;
            }
            self.bump();
        }
        self.line_end(from);
    }

    /// The last consumed token is a `Dedent` consumed after token index `from`: the
    /// statement begun there ended with its block.
    fn closed_block_since(&self, from: usize) -> bool {
        self.pos > from && self.toks[self.pos - 1].kind == TokKind::Dedent
    }

    /// `line_end = Newline | Eof | ε after a Dedent`: anything else is 2000 and the rest of the
    /// statement is skipped.
    fn line_end(&mut self, from: usize) {
        match self.peek().kind {
            TokKind::Newline | TokKind::Eof | TokKind::Dedent => {}
            _ if self.closed_block_since(from) => {}
            // A recovery's forced progress (rule 1) consumed this line's `Newline`.
            _ if self.pos > from && self.toks[self.pos - 1].kind == TokKind::Newline => {}
            _ => {
                let at = self.tspan(self.peek());
                self.recover(code::EXPECTED, at, "expected the end of the line");
                self.skip_statement();
            }
        }
    }

    /// One statement (the §4.7 dispatch table).
    fn stmt(&mut self) -> NodeId {
        let t = self.peek();
        match t.kind {
            TokKind::Break | TokKind::Continue => {
                self.bump();
                if self.loop_depth == 0 {
                    return self.recover(
                        code::LOOP_CONTROL_OUTSIDE_LOOP,
                        self.tspan(t),
                        "`break`/`continue` outside a loop",
                    );
                }
                let kind = if t.kind == TokKind::Break {
                    NodeKind::Break
                } else {
                    NodeKind::Continue
                };
                self.build.leaf(kind, 0, 0, 0, self.tspan(t))
            }
            TokKind::If | TokKind::Switch | TokKind::For | TokKind::While => self.structure(),
            TokKind::Var | TokKind::Varip => {
                self.bump();
                let mode = if t.kind == TokKind::Var {
                    DeclMode::Var
                } else {
                    DeclMode::Varip
                };
                if self.peek().kind == TokKind::LBracket {
                    let at = self.tspan(self.peek());
                    let e = self.recover(
                        code::MODE_ON_TUPLE,
                        at,
                        "tuple declarations do not support extra keywords",
                    );
                    self.skip_statement();
                    return e;
                }
                self.var_decl(t.lo, mode)
            }
            TokKind::LBracket if self.tuple_is_declaration() => self.tuple_decl(),
            TokKind::Ident => {
                let next = self.peek_at(1).kind;
                let after = self.peek_at(2).kind;
                let header_end = matches!(after, TokKind::Newline | TokKind::Eof);
                match t.kw {
                    SoftKw::Import if next == TokKind::Seg => return self.import_decl(),
                    SoftKw::Export if next == TokKind::Ident => return self.export_decl(),
                    SoftKw::Type if next == TokKind::Ident && header_end => {
                        return self.type_decl(t.lo, 0)
                    }
                    SoftKw::Enum if next == TokKind::Ident && header_end => {
                        return self.enum_decl(t.lo, 0)
                    }
                    SoftKw::Method if next == TokKind::Ident && after == TokKind::LParen => {
                        self.bump();
                        return self.func_decl(t.lo, crate::ast::nf::METHOD);
                    }
                    _ => {}
                }
                if self.is_function_declaration() {
                    return self.func_decl(t.lo, 0);
                }
                if self.is_once_header() {
                    return self.once_stmt();
                }
                if self.is_typed_declaration() {
                    return self.var_decl(t.lo, DeclMode::Plain);
                }
                if self.peek_at(1).kind == TokKind::Eq {
                    return self.var_decl(t.lo, DeclMode::Plain);
                }
                self.expr_statement()
            }
            _ => self.expr_statement(),
        }
    }

    /// An expression; then `:=`/compound → `Assign` (target `Ident` or `Field`, else 2021);
    /// `=` after a field is 2021.
    fn expr_statement(&mut self) -> NodeId {
        let lhs = self.expr(0);
        if self.recovering {
            return lhs;
        }
        let t = self.peek();
        let op = match t.kind {
            TokKind::ColonEq => AssignOp::Reassign,
            TokKind::PlusEq => AssignOp::Add,
            TokKind::MinusEq => AssignOp::Sub,
            TokKind::StarEq => AssignOp::Mul,
            TokKind::SlashEq => AssignOp::Div,
            TokKind::PercentEq => AssignOp::Rem,
            TokKind::Eq => {
                return self.recover(
                    code::BAD_ASSIGN_TARGET,
                    self.tspan(t),
                    "`=` declares a variable; a field is reassigned with `:=`",
                );
            }
            _ => return lhs,
        };
        if !matches!(self.build.kind(lhs), NodeKind::Ident | NodeKind::Field) {
            let at = self.build.span(lhs);
            return self.recover(code::BAD_ASSIGN_TARGET, at, "this cannot be assigned to");
        }
        self.bump();
        let value = self.rhs();
        let span = Span::new(self.file, self.build.span(lhs).lo, self.prev_hi());
        self.alloc(NodeKind::Assign, 0, [lhs.0, value.0, op as u32, 0], span)
    }

    /// `tuple_decl = "[" NAME { "," NAME } "]" "=" rhs` and `tuple_assign` with `:=` (L5).
    fn tuple_decl(&mut self) -> NodeId {
        let close = self
            .matching(TokKind::LBracket, TokKind::RBracket)
            .unwrap_or(0);
        let assign = self.peek_at(close + 1).kind == TokKind::ColonEq;
        let binder = if assign {
            NodeKind::Ident
        } else {
            NodeKind::Name
        };
        let open = self.bump();
        let mark = self.build.mark();
        loop {
            let n = self.peek();
            if n.kind != TokKind::Ident {
                let at = self.tspan(n);
                return self.recover(code::EXPECTED_IDENT, at, "expected a name in the tuple");
            }
            if self.peek_at(1).kind == TokKind::Ident {
                let at = self.tspan(n);
                return self.recover(
                    code::MODE_ON_TUPLE,
                    at,
                    "tuple declarations do not support types or qualifiers",
                );
            }
            self.bump();
            let b = self.build.leaf(binder, 0, n.sym.0, 0, self.tspan(n));
            self.build.item(b);
            if self.peek().kind != TokKind::Comma {
                break;
            }
            self.bump();
        }
        if self.peek().kind != TokKind::RBracket {
            let at = self.tspan(self.peek());
            return self.recover(code::EXPECTED_IDENT, at, "expected `,` or `]`");
        }
        self.bump();
        let op = self.bump();
        let value = self.rhs();
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, open.lo, self.prev_hi());
        debug_assert_eq!(op.kind == TokKind::ColonEq, assign);
        let kind = if assign {
            NodeKind::TupleAssign
        } else {
            NodeKind::TupleDecl
        };
        self.alloc(kind, 0, [start, len, value.0, 0], span)
    }

    /// `once = "once" [ expr ] block` (SD4: v6 only).
    fn once_stmt(&mut self) -> NodeId {
        let t = self.bump();
        if self.dialect == pine_ir::Dialect::V5 {
            self.diags.at(
                code::CONSTRUCT_NOT_IN_DIALECT,
                self.file,
                t.lo,
                t.hi,
                "`once` is v6-only",
            );
        }
        let mut cond = NodeId::NONE;
        if self.peek().kind != TokKind::Newline {
            cond = self.expr(0);
        }
        let body = self.block();
        let span = Span::new(self.file, t.lo, self.prev_hi());
        self.alloc(NodeKind::Once, 0, [cond.0, body.0, 0, 0], span)
    }

    /// `rhs = if | switch | for | for_in | while | expr`.
    pub(super) fn rhs(&mut self) -> NodeId {
        match self.peek().kind {
            TokKind::If | TokKind::Switch | TokKind::For | TokKind::While => self.structure(),
            _ => self.expr(0),
        }
    }

    /// `var_decl = [ "var" | "varip" ] [ QUAL ] [ type ] NAME "=" rhs` (after the mode word).
    pub(super) fn var_decl(&mut self, lo: u32, mode: DeclMode) -> NodeId {
        let mut qual: u32 = 0;
        let t = self.peek();
        if t.kind == TokKind::Ident && self.peek_at(1).kind == TokKind::Ident {
            let q = match t.kw {
                SoftKw::Const => Some(Qual::Const),
                SoftKw::Simple => Some(Qual::Simple),
                SoftKw::Series => Some(Qual::Series),
                _ => None,
            };
            if let Some(q) = q {
                self.bump();
                qual = q as u32 + 1;
            }
        }
        let mut ty = NodeId::NONE;
        if self.is_typed_declaration() {
            ty = self.type_ref();
        }
        let n = self.peek();
        if n.kind != TokKind::Ident {
            let at = self.tspan(n);
            return self.recover(code::EXPECTED_IDENT, at, "expected a variable name");
        }
        self.bump();
        let name = self
            .build
            .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
        if self.peek().kind != TokKind::Eq {
            let at = self.tspan(self.peek());
            return self.recover(code::EXPECTED, at, "expected `=` and a value");
        }
        self.bump();
        let init = self.rhs();
        let span = Span::new(self.file, lo, self.prev_hi());
        let d = mode as u32 | (qual << 8);
        self.alloc(NodeKind::VarDecl, 0, [ty.0, name.0, init.0, d], span)
    }

    /// Lookahead 1: `[QUAL] path [ <…> | [ ] ] WORD =` within 64 tokens.
    pub(super) fn is_typed_declaration(&self) -> bool {
        let t = self.peek_at(0);
        let quals = matches!(t.kw, SoftKw::Const | SoftKw::Simple | SoftKw::Series);
        let k = usize::from(
            t.kind == TokKind::Ident && quals && self.peek_at(1).kind == TokKind::Ident,
        );
        match self.type_end(k) {
            Some(k) => {
                self.peek_at(k).kind == TokKind::Ident && self.peek_at(k + 1).kind == TokKind::Eq
            }
            None => false,
        }
    }

    /// The offset just past a type `path [ <…> | [ ] ]` starting at offset `k` (index-only, at
    /// most 64 tokens), or `None`.
    pub(super) fn type_end(&self, mut k: usize) -> Option<usize> {
        if self.peek_at(k).kind != TokKind::Ident {
            return None;
        }
        k += 1;
        while self.peek_at(k).kind == TokKind::Dot && self.peek_at(k + 1).kind == TokKind::Ident {
            k += 2;
        }
        if self.peek_at(k).kind == TokKind::Lt {
            let mut depth = 1u32;
            k += 1;
            while depth > 0 {
                if k > TYPED_DECL_SCAN {
                    return None;
                }
                match self.peek_at(k).kind {
                    TokKind::Ident | TokKind::Dot | TokKind::Comma => {}
                    TokKind::Lt => depth += 1,
                    TokKind::Gt => depth -= 1,
                    TokKind::LBracket if self.peek_at(k + 1).kind == TokKind::RBracket => k += 1,
                    _ => return None,
                }
                k += 1;
            }
        }
        if self.pol.array_suffix_type
            && self.peek_at(k).kind == TokKind::LBracket
            && self.peek_at(k + 1).kind == TokKind::RBracket
        {
            k += 2;
        }
        Some(k)
    }

    /// Lookahead 2: `[` … matching `]` followed by `=` or `:=`.
    fn tuple_is_declaration(&self) -> bool {
        match self.matching(TokKind::LBracket, TokKind::RBracket) {
            Some(k) => matches!(self.peek_at(k + 1).kind, TokKind::Eq | TokKind::ColonEq),
            None => false,
        }
    }

    /// Lookahead 3: WORD `(` … matching `)` `=>`.
    pub(super) fn is_function_declaration(&self) -> bool {
        if self.peek_at(1).kind != TokKind::LParen {
            return false;
        }
        let save = 1usize;
        match self.matching_from(save, TokKind::LParen, TokKind::RParen) {
            Some(k) => self.peek_at(k + 1).kind == TokKind::FatArrow,
            None => false,
        }
    }

    /// Lookahead 4: `once` whose logical line is followed by `Indent`.
    fn is_once_header(&self) -> bool {
        let t = self.peek();
        if t.kw != SoftKw::Once || t.flags & LINE_START == 0 {
            return false;
        }
        let mut k = 1usize;
        loop {
            match self.peek_at(k).kind {
                TokKind::Newline => return self.peek_at(k + 1).kind == TokKind::Indent,
                TokKind::Eof | TokKind::Dedent | TokKind::Indent => return false,
                _ => k += 1,
            }
        }
    }

    /// The offset (from the cursor) of the closer matching the opener at the cursor.
    fn matching(&self, open: TokKind, close: TokKind) -> Option<usize> {
        self.matching_from(0, open, close)
    }

    /// The offset of the closer matching the opener at offset `k0`; `None` at the line's end.
    fn matching_from(&self, k0: usize, open: TokKind, close: TokKind) -> Option<usize> {
        let mut depth = 0u32;
        let mut k = k0;
        loop {
            let kind = self.peek_at(k).kind;
            if kind == open {
                depth += 1;
            } else if kind == close {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            } else if matches!(kind, TokKind::Newline | TokKind::Eof) {
                return None;
            }
            k += 1;
        }
    }

    // ---- structures -------------------------------------------------------------------------

    /// `if`, `switch`, `for`/`for … in`, `while`.
    fn structure(&mut self) -> NodeId {
        let t = self.peek();
        match t.kind {
            TokKind::If => self.if_stmt(),
            TokKind::While => {
                self.bump();
                let cond = self.expr(0);
                let body = self.loop_block();
                let span = Span::new(self.file, t.lo, self.prev_hi());
                self.alloc(NodeKind::While, 0, [cond.0, body.0, 0, 0], span)
            }
            TokKind::For => self.for_stmt(),
            _ => self.switch_stmt(),
        }
    }

    /// `if = "if" expr block [ "else" ( if | block ) ]`.
    fn if_stmt(&mut self) -> NodeId {
        let from = self.pos;
        let t = self.bump();
        let cond = self.expr(0);
        let then = self.block();
        let mut otherwise = NodeId::NONE;
        if self.peek().kind == TokKind::Else && self.closed_block_since(from) {
            self.bump();
            otherwise = if self.peek().kind == TokKind::If {
                self.if_stmt()
            } else {
                self.block()
            };
        }
        let span = Span::new(self.file, t.lo, self.prev_hi());
        self.alloc(NodeKind::If, 0, [cond.0, then.0, otherwise.0, 0], span)
    }

    /// `for` and `for … in`.
    fn for_stmt(&mut self) -> NodeId {
        let t = self.bump();
        let p0 = self.peek();
        let tuple = p0.kind == TokKind::LBracket;
        let single_in = p0.kind == TokKind::Ident && self.peek_at(1).kw == SoftKw::In;
        if tuple || single_in {
            let mark = self.build.mark();
            if tuple {
                self.bump();
                loop {
                    let n = self.peek();
                    if n.kind != TokKind::Ident {
                        let at = self.tspan(n);
                        return self.recover(code::EXPECTED_IDENT, at, "expected a loop variable");
                    }
                    self.bump();
                    let name = self
                        .build
                        .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
                    self.build.item(name);
                    if self.peek().kind != TokKind::Comma {
                        break;
                    }
                    self.bump();
                }
                self.expect_close(TokKind::RBracket, "expected `]`");
            } else {
                let n = self.bump();
                let name = self
                    .build
                    .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
                self.build.item(name);
            }
            if self.peek().kw != SoftKw::In {
                let at = self.tspan(self.peek());
                return self.recover(code::EXPECTED, at, "expected `in`");
            }
            self.bump();
            let coll = self.expr(0);
            let body = self.loop_block();
            let (start, len) = self.build.close_list(mark);
            let span = Span::new(self.file, t.lo, self.prev_hi());
            return self.alloc(NodeKind::ForIn, 0, [start, len, coll.0, body.0], span);
        }
        let mut ty = NodeId::NONE;
        if p0.kind == TokKind::Ident && self.peek_at(1).kind == TokKind::Ident {
            ty = self.type_ref();
        }
        let n = self.peek();
        if n.kind != TokKind::Ident {
            let at = self.tspan(n);
            return self.recover(code::EXPECTED_IDENT, at, "expected a loop counter");
        }
        self.bump();
        let counter = self
            .build
            .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
        if self.peek().kind != TokKind::Eq {
            let at = self.tspan(self.peek());
            return self.recover(code::EXPECTED, at, "expected `=`");
        }
        self.bump();
        let from = self.expr(0);
        if self.peek().kw != SoftKw::To {
            let at = self.tspan(self.peek());
            return self.recover(code::EXPECTED, at, "expected `to`");
        }
        self.bump();
        let to = self.expr(0);
        let mut step = NodeId::NONE;
        if self.peek().kw == SoftKw::By {
            self.bump();
            step = self.expr(0);
        }
        let body = self.loop_block();
        let mark = self.build.mark();
        let mut k = 0usize;
        let words = [ty, counter, from, to, step, body];
        while k < words.len() {
            self.build.item(words[k]);
            k += 1;
        }
        let (start, _) = self.build.close_list(mark);
        let span = Span::new(self.file, t.lo, self.prev_hi());
        self.alloc(NodeKind::For, 0, [start, 0, 0, 0], span)
    }

    /// `switch = "switch" [ expr ] Newline Indent arm { arm } Dedent`.
    fn switch_stmt(&mut self) -> NodeId {
        let t = self.bump();
        let mut subject = NodeId::NONE;
        if self.peek().kind != TokKind::Newline {
            subject = self.expr(0);
        }
        if self.peek().kind != TokKind::Newline || self.peek_at(1).kind != TokKind::Indent {
            let at = self.tspan(self.peek());
            return self.recover(code::MISSING_BLOCK, at, "`switch` needs its indented arms");
        }
        self.bump();
        self.bump();
        let mark = self.build.mark();
        let mut default: Option<Span> = None;
        loop {
            match self.peek().kind {
                TokKind::Dedent => {
                    self.bump();
                    break;
                }
                TokKind::Eof => break,
                TokKind::Newline => {
                    self.bump();
                    continue;
                }
                _ => {}
            }
            self.recovering = false;
            let a = self.peek();
            let mut pattern = NodeId::NONE;
            if a.kind == TokKind::FatArrow {
                let here = self.tspan(a);
                if let Some(first) = default {
                    self.report_node(
                        code::DUPLICATE_DEFAULT_ARM,
                        here,
                        "a second default arm",
                        first,
                        "the first default arm",
                    );
                }
                default.get_or_insert(here);
            } else {
                if let Some(first) = default {
                    self.report_node(
                        code::ARM_AFTER_DEFAULT,
                        self.tspan(a),
                        "an arm after the default arm",
                        first,
                        "the default arm",
                    );
                }
                pattern = self.expr(0);
            }
            if self.peek().kind != TokKind::FatArrow {
                let at = self.tspan(self.peek());
                let e = self.recover(code::EXPECTED, at, "expected `=>`");
                self.skip_statement();
                self.build.item(e);
                continue;
            }
            self.bump();
            let body = self.body();
            let span = Span::new(self.file, a.lo, self.prev_hi());
            let arm = self.alloc(NodeKind::SwitchArm, 0, [pattern.0, body.0, 0, 0], span);
            self.build.item(arm);
        }
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, t.lo, self.prev_hi());
        self.alloc(NodeKind::Switch, 0, [subject.0, start, len, 0], span)
    }

    /// A loop body: a block with `loop_depth` raised.
    fn loop_block(&mut self) -> NodeId {
        self.loop_depth += 1;
        let b = self.block();
        self.loop_depth -= 1;
        b
    }

    /// `block = Newline Indent line { line } Dedent` after a header; else 2005 (or 2000 for
    /// trailing tokens on the header line) and an `Error` body.
    fn block(&mut self) -> NodeId {
        let t = self.peek();
        let failed = t.kind != TokKind::Newline;
        if failed {
            if !self.recovering {
                self.recover(
                    code::EXPECTED,
                    self.tspan(t),
                    "expected the end of the header",
                );
            }
            self.skip_to_line_end();
        }
        if self.peek().kind != TokKind::Newline || self.peek_at(1).kind != TokKind::Indent {
            let at = self.tspan(self.peek());
            return self.recover(
                code::MISSING_BLOCK,
                at,
                "a structure header needs an indented block",
            );
        }
        self.bump();
        let indent = self.bump();
        let mark = self.build.mark();
        self.scope_depth += 1;
        self.lines(true);
        self.scope_depth -= 1;
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, indent.hi, self.prev_hi());
        self.alloc(NodeKind::Block, 0, [start, len, 0, 0], span)
    }

    /// Consume the rest of the logical line (never its `Newline`).
    pub(super) fn skip_to_line_end(&mut self) {
        while !matches!(
            self.peek().kind,
            TokKind::Newline | TokKind::Eof | TokKind::Indent | TokKind::Dedent
        ) {
            self.bump();
        }
    }

    /// `body = block | stmt { "," stmt } Newline` after `=>`; both are `Block`s.
    pub(super) fn body(&mut self) -> NodeId {
        if self.peek().kind == TokKind::Newline {
            return self.block();
        }
        let lo = self.peek().lo;
        let from = self.pos;
        let mark = self.build.mark();
        self.scope_depth += 1;
        loop {
            let s = self.stmt();
            self.build.item(s);
            if self.peek().kind != TokKind::Comma || self.closed_block_since(from) {
                break;
            }
            self.bump();
        }
        self.scope_depth -= 1;
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, lo, self.prev_hi());
        self.alloc(NodeKind::Block, 0, [start, len, 0, 0], span)
    }
}
