// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The Pratt engine and the §4.6 table: `nud`/`led`, tuples versus history, the type-argument
//! scan, argument lists and type references.

use pine_ir::{NodeId, Span};

use super::Parser;
use crate::ast::{nf, BinOp, NodeKind, UnOp};
use crate::code;
use crate::lex::literal::int_value;
use crate::lex::token::{TokKind, HAS_ALPHA, STR_FORM_SHIFT};
use crate::MAX_DEPTH;

/// Binding power of history, calls, members and type-argument calls.
const BP_POSTFIX: u8 = 90;
/// Right binding power of the prefix operators.
const BP_PREFIX: u8 = 80;
/// Binding power of `? :`.
const BP_TERNARY: u8 = 10;
/// Most tokens the type-argument scan reads.
const TYPE_ARG_SCAN: usize = 64;

/// The infix operator and its binding power, for a token.
fn infix(k: TokKind) -> Option<(BinOp, u8)> {
    Some(match k {
        TokKind::Star => (BinOp::Mul, 70),
        TokKind::Slash => (BinOp::Div, 70),
        TokKind::Percent => (BinOp::Rem, 70),
        TokKind::Plus => (BinOp::Add, 60),
        TokKind::Minus => (BinOp::Sub, 60),
        TokKind::Lt => (BinOp::Lt, 50),
        TokKind::Le => (BinOp::Le, 50),
        TokKind::Gt => (BinOp::Gt, 50),
        TokKind::Ge => (BinOp::Ge, 50),
        TokKind::EqEq => (BinOp::Eq, 40),
        TokKind::BangEq => (BinOp::Ne, 40),
        TokKind::And => (BinOp::And, 30),
        TokKind::Or => (BinOp::Or, 20),
        _ => return None,
    })
}

impl Parser<'_, '_> {
    /// `expr(min_bp)`: the Pratt loop.
    pub(super) fn expr(&mut self, min_bp: u8) -> NodeId {
        if self.depth >= MAX_DEPTH {
            let at = self.tspan(self.peek());
            return self.recover(
                code::NESTING_TOO_DEEP,
                at,
                "an expression nested too deeply",
            );
        }
        self.depth += 1;
        let mut lhs = self.nud();
        // Rule 2: once this statement is recovering, its rest is the statement recovery's.
        while !self.recovering {
            let lbp = self.lbp(lhs);
            if lbp <= min_bp {
                break;
            }
            lhs = self.led(lhs, lbp);
        }
        self.depth -= 1;
        lhs
    }

    /// The left binding power of the current token after `lhs`.
    fn lbp(&self, lhs: NodeId) -> u8 {
        let k = self.peek().kind;
        match k {
            TokKind::LBracket | TokKind::LParen | TokKind::Dot => BP_POSTFIX,
            TokKind::Question => BP_TERNARY,
            TokKind::Lt if self.is_type_arg_call(lhs) => BP_POSTFIX,
            _ => infix(k).map_or(0, |(_, bp)| bp),
        }
    }

    /// `true` when the `<` at the cursor opens a type-argument list of a call on `lhs` (§4.6).
    fn is_type_arg_call(&self, lhs: NodeId) -> bool {
        if !matches!(self.build.kind(lhs), NodeKind::Ident | NodeKind::Field) {
            return false;
        }
        let mut depth = 1u32;
        let mut k = 1usize;
        while k <= TYPE_ARG_SCAN {
            match self.peek_at(k).kind {
                TokKind::Ident | TokKind::Dot | TokKind::Comma => {}
                TokKind::Lt => depth += 1,
                TokKind::Gt => {
                    depth -= 1;
                    if depth == 0 {
                        return self.peek_at(k + 1).kind == TokKind::LParen;
                    }
                }
                TokKind::LBracket
                    if self.pol.array_suffix_type
                        && self.peek_at(k + 1).kind == TokKind::RBracket =>
                {
                    k += 1;
                }
                _ => return false,
            }
            k += 1;
        }
        false
    }

    /// The prefix position.
    fn nud(&mut self) -> NodeId {
        let t = self.peek();
        let at = self.tspan(t);
        match t.kind {
            TokKind::Int => {
                self.bump();
                let v = int_value(&self.src[t.lo as usize..t.hi as usize])
                    .expect("pine-lang bug: the lexer accepted the integer literal");
                self.build
                    .leaf(NodeKind::IntLit, 0, v as u32, (v >> 32) as u32, at)
            }
            TokKind::Float => {
                self.bump();
                let text = core::str::from_utf8(&self.src[t.lo as usize..t.hi as usize])
                    .expect("pine-lang bug: a Float token spans ASCII bytes only");
                let v: f64 = text
                    .parse()
                    .expect("pine-lang bug: the lexer accepted the float");
                let bits = v.to_bits();
                self.build
                    .leaf(NodeKind::FloatLit, 0, bits as u32, (bits >> 32) as u32, at)
            }
            TokKind::Str => {
                self.bump();
                let form = (t.flags >> STR_FORM_SHIFT) & 3;
                self.build
                    .leaf(NodeKind::StrLit, form << nf::FORM_SHIFT, t.sym.0, 0, at)
            }
            TokKind::Color => {
                self.bump();
                let d = &self.src[t.lo as usize + 1..t.hi as usize];
                let rgb = hex(&d[..6]);
                if t.flags & HAS_ALPHA != 0 {
                    self.build
                        .leaf(NodeKind::ColorLit, nf::HAS_ALPHA, rgb, hex(&d[6..8]), at)
                } else {
                    self.build.leaf(NodeKind::ColorLit, 0, rgb, 0, at)
                }
            }
            TokKind::True | TokKind::False => {
                self.bump();
                self.build.leaf(
                    NodeKind::BoolLit,
                    0,
                    u32::from(t.kind == TokKind::True),
                    0,
                    at,
                )
            }
            TokKind::Ident => {
                self.bump();
                self.build.leaf(NodeKind::Ident, 0, t.sym.0, 0, at)
            }
            TokKind::LParen => {
                self.bump();
                let e = self.expr(0);
                self.expect_close(TokKind::RParen, "expected `)`");
                e
            }
            TokKind::LBracket => self.tuple(),
            TokKind::Plus | TokKind::Minus | TokKind::Not => {
                self.bump();
                let op = match t.kind {
                    TokKind::Plus => UnOp::Pos,
                    TokKind::Minus => UnOp::Neg,
                    _ => UnOp::Not,
                };
                let operand = self.expr(BP_PREFIX);
                let span = Span::new(self.file, t.lo, self.build.span(operand).hi.max(t.hi));
                self.alloc(NodeKind::Unary, 0, [operand.0, op as u32, 0, 0], span)
            }
            TokKind::If | TokKind::Switch | TokKind::For | TokKind::While => {
                let e = self.recover(
                    code::STRUCTURE_NOT_ALLOWED_HERE,
                    at,
                    "a structure cannot be an operand; it is a statement or the right side of \
                     `=`/`:=`",
                );
                self.skip_statement();
                e
            }
            TokKind::Error => {
                self.bump();
                if self.recovering {
                    return self.last_error;
                }
                // Already reported by the lexer: a silent Error node, and this statement is in
                // recovery so nothing else at it is reported.
                self.recovering = true;
                self.last_error = self.build.leaf(NodeKind::Error, 0, 0, 0, at);
                self.last_error
            }
            // A wrap at a multiple of four outside parentheses (R3): the layout machine made it
            // a block, so the operand is missing here — reported once, as 1008 at the indent.
            TokKind::Newline if self.peek_at(1).kind == TokKind::Indent => {
                let at = self.tspan(self.peek_at(1));
                self.recover(
                    code::UNEXPECTED_INDENT,
                    at,
                    "a wrapped line outside parentheses cannot be indented by a multiple of four",
                )
            }
            _ => self.recover(code::EXPECTED_EXPRESSION, at, "expected an expression"),
        }
    }

    /// The infix/postfix position, with `lbp` from `lbp(lhs)`.
    fn led(&mut self, lhs: NodeId, lbp: u8) -> NodeId {
        let t = self.peek();
        let lo = self.build.span(lhs).lo;
        match t.kind {
            TokKind::Question => {
                self.bump();
                let then = self.expr(0);
                if self.peek().kind != TokKind::Colon {
                    let at = self.tspan(self.peek());
                    return self.recover(code::EXPECTED, at, "expected `:` in a conditional");
                }
                self.bump();
                // pin:01-ternary-assoc.
                let rbp = if self.pol.ternary_right_assoc {
                    lbp - 1
                } else {
                    lbp
                };
                let otherwise = self.expr(rbp);
                let span = Span::new(self.file, lo, self.build.span(otherwise).hi);
                self.alloc(NodeKind::Ternary, 0, [lhs.0, then.0, otherwise.0, 0], span)
            }
            TokKind::LParen => {
                let mark = self.build.mark();
                self.call(lhs, mark, 0)
            }
            TokKind::Lt if lbp == BP_POSTFIX => {
                // `lbp` chose the type-argument reading (§4.6 scan).
                let mark = self.build.mark();
                self.bump();
                let n = self.type_list(mark);
                self.call(lhs, mark, n)
            }
            TokKind::LBracket => {
                self.bump();
                let offset = self.expr(0);
                let hi = self.expect_close(TokKind::RBracket, "expected `]`");
                let span = Span::new(self.file, lo, hi);
                self.alloc(NodeKind::Hist, 0, [lhs.0, offset.0, 0, 0], span)
            }
            TokKind::Dot => {
                self.bump();
                let m = self.peek();
                if m.kind != TokKind::Ident {
                    let at = self.tspan(m);
                    return self.recover(
                        code::EXPECTED_IDENT,
                        at,
                        "expected a member name after `.`",
                    );
                }
                self.bump();
                let name = self
                    .build
                    .leaf(NodeKind::Name, 0, m.sym.0, 0, self.tspan(m));
                let span = Span::new(self.file, lo, m.hi);
                self.alloc(NodeKind::Field, 0, [lhs.0, name.0, 0, 0], span)
            }
            _ => {
                let (op, bp) = infix(t.kind).unwrap_or((BinOp::Add, 0));
                debug_assert!(bp == lbp);
                self.bump();
                let rhs = self.expr(bp);
                let span = Span::new(self.file, lo, self.build.span(rhs).hi);
                self.alloc(NodeKind::Binary, 0, [lhs.0, rhs.0, op as u32, 0], span)
            }
        }
    }

    /// `[` items `]` → `Tuple` (empty → 2002).
    fn tuple(&mut self) -> NodeId {
        let open = self.bump();
        if self.peek().kind == TokKind::RBracket {
            let close = self.bump();
            let at = Span::new(self.file, open.lo, close.hi);
            return self.recover(
                code::EXPECTED_EXPRESSION,
                at,
                "a tuple needs at least one item",
            );
        }
        let mark = self.build.mark();
        self.in_list += 1;
        loop {
            let e = self.expr(0);
            self.build.item(e);
            if self.peek().kind != TokKind::Comma {
                break;
            }
            self.bump();
        }
        self.in_list = self.in_list.saturating_sub(1);
        let hi = self.expect_close(TokKind::RBracket, "expected `,` or `]`");
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, open.lo, hi);
        self.alloc(NodeKind::Tuple, 0, [start, len, 0, 0], span)
    }

    /// A call on `callee` at `(`; `ntype` type arguments are already on the list stack above
    /// `mark`. `(` [ arg { `,` arg } [ `,` ] ] `)`; `WORD =` starts a `NamedArg`.
    fn call(&mut self, callee: NodeId, mark: usize, ntype: u32) -> NodeId {
        let lo = self.build.span(callee).lo;
        if self.peek().kind != TokKind::LParen {
            let at = self.tspan(self.peek());
            return self.recover(code::EXPECTED, at, "expected `(`");
        }
        self.bump();
        self.in_list += 1;
        while self.peek().kind != TokKind::RParen {
            let arg = self.arg();
            self.build.item(arg);
            if self.peek().kind != TokKind::Comma {
                break;
            }
            self.bump();
        }
        self.in_list = self.in_list.saturating_sub(1);
        let hi = self.expect_close(TokKind::RParen, "expected `,` or `)`");
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, lo, hi);
        self.alloc(
            NodeKind::Call,
            0,
            [callee.0, start, ntype, len - ntype],
            span,
        )
    }

    /// One argument: `NAME = expr` or `expr`.
    fn arg(&mut self) -> NodeId {
        let t = self.peek();
        if t.kind == TokKind::Ident && self.peek_at(1).kind == TokKind::Eq {
            self.bump();
            self.bump();
            let name = self
                .build
                .leaf(NodeKind::Name, 0, t.sym.0, 0, self.tspan(t));
            let value = self.expr(0);
            let span = Span::new(self.file, t.lo, self.build.span(value).hi);
            return self.alloc(NodeKind::NamedArg, 0, [name.0, value.0, 0, 0], span);
        }
        self.expr(0)
    }

    /// After `<`: `type { , type } >`, each pushed above `mark`; returns how many.
    fn type_list(&mut self, mark: usize) -> u32 {
        loop {
            let ty = self.type_ref();
            self.build.item(ty);
            if self.peek().kind != TokKind::Comma {
                break;
            }
            self.bump();
        }
        if self.peek().kind == TokKind::Gt {
            self.bump();
        } else {
            let at = self.tspan(self.peek());
            self.recover(
                code::UNCLOSED_TYPE_ARGS,
                at,
                "expected `>` closing the type arguments",
            );
        }
        (self.build.mark() - mark) as u32
    }

    /// `type = path [ < type { , type } > ] [ [ ] ]`, path a dotted word chain.
    pub(super) fn type_ref(&mut self) -> NodeId {
        let t = self.peek();
        if t.kind != TokKind::Ident {
            let at = self.tspan(t);
            return self.recover(code::EXPECTED_TYPE, at, "expected a type");
        }
        self.bump();
        let mut path = self
            .build
            .leaf(NodeKind::Ident, 0, t.sym.0, 0, self.tspan(t));
        while self.peek().kind == TokKind::Dot && self.peek_at(1).kind == TokKind::Ident {
            self.bump();
            let m = self.bump();
            let name = self
                .build
                .leaf(NodeKind::Name, 0, m.sym.0, 0, self.tspan(m));
            let span = Span::new(self.file, t.lo, m.hi);
            path = self.alloc(NodeKind::Field, 0, [path.0, name.0, 0, 0], span);
        }
        let mark = self.build.mark();
        let mut n = 0u32;
        if self.peek().kind == TokKind::Lt {
            self.bump();
            n = self.type_list(mark);
        }
        let mut flags = 0u8;
        if self.pol.array_suffix_type
            && self.peek().kind == TokKind::LBracket
            && self.peek_at(1).kind == TokKind::RBracket
        {
            self.bump();
            self.bump();
            flags |= nf::ARRAY_SUFFIX;
        }
        let (start, len) = self.build.close_list(mark);
        debug_assert_eq!(len, n);
        let span = Span::new(self.file, t.lo, self.prev_hi());
        self.alloc(NodeKind::TypeRef, flags, [path.0, start, len, 0], span)
    }

    /// Consume the closer `k` and return its `hi`; otherwise 2000 (silent at the end of the line,
    /// where the lexer's 1010 already reported the open bracket) and the current position.
    pub(super) fn expect_close(&mut self, k: TokKind, msg: &'static str) -> u32 {
        let t = self.peek();
        if t.kind == k {
            self.bump();
            return t.hi;
        }
        match t.kind {
            // Only an unclosed bracket lets the line end inside one: the lexer's 1010 reported
            // it, so the recovery is silent (rule 3, §4.8).
            TokKind::Newline | TokKind::Dedent | TokKind::Eof => {
                if !self.recovering {
                    self.recovering = true;
                    self.last_error = self.build.leaf(NodeKind::Error, 0, 0, 0, self.tspan(t));
                }
            }
            _ => {
                self.recover(code::EXPECTED, self.tspan(t), msg);
            }
        }
        self.prev_hi()
    }
}

/// The value of ASCII hex digits (the lexer verified them).
fn hex(d: &[u8]) -> u32 {
    let mut v = 0u32;
    let mut i = 0usize;
    while i < d.len() {
        let c = d[i];
        let x = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => c - b'A' + 10,
        };
        v = (v << 4) | u32::from(x);
        i += 1;
    }
    v
}
