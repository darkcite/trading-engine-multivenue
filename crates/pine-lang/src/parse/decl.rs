// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Declarations (§4.7): functions, methods, parameters, `type` + fields, `enum` + members,
//! `import`, `export`, and the positional global-scope check (2027).

use pine_ir::{NodeId, Qual, Span};

use super::Parser;
use crate::ast::{nf, DeclMode, NodeKind};
use crate::code;
use crate::lex::token::{SoftKw, TokKind};

impl Parser<'_, '_> {
    /// 2027 when the declaration that began at `lo` is not at global scope; `local_ok` is the
    /// `local_type_decl` pin for `type`/`enum`/`import`/`export` (functions are never local).
    fn global_only(&mut self, decl: NodeId, lo: u32, local_ok: bool) -> NodeId {
        if self.scope_depth == 0 || local_ok {
            return decl;
        }
        let at = Span::new(self.file, lo, self.prev_hi());
        self.recover(
            code::DECL_NOT_GLOBAL,
            at,
            "this declaration is allowed at global scope only",
        )
    }

    /// `func = NAME "(" [ param { "," param } [","] ] ")" "=>" body`; `flags` carries METHOD
    /// (the `method` word already consumed) and EXPORT.
    pub(super) fn func_decl(&mut self, lo: u32, flags: u8) -> NodeId {
        let n = self.bump();
        let name = self
            .build
            .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
        if self.peek().kind != TokKind::LParen {
            let at = self.tspan(self.peek());
            return self.recover(code::EXPECTED, at, "expected `(`");
        }
        self.bump();
        let mark = self.build.mark();
        while self.peek().kind != TokKind::RParen {
            let p = self.param();
            self.build.item(p);
            if self.recovering || self.peek().kind != TokKind::Comma {
                break;
            }
            self.bump();
        }
        self.expect_close(TokKind::RParen, "expected `,` or `)`");
        if self.peek().kind != TokKind::FatArrow {
            let at = self.tspan(self.peek());
            return self.recover(code::EXPECTED, at, "expected `=>`");
        }
        self.bump();
        let saved_loops = self.loop_depth;
        self.loop_depth = 0;
        let body = match self.peek().kind {
            TokKind::Newline if self.peek_at(1).kind != TokKind::Indent => {
                let at = self.tspan(self.peek());
                self.recover(
                    code::BAD_FUNCTION_BODY,
                    at,
                    "`=>` needs an inline body or a block",
                )
            }
            TokKind::Eof => {
                let at = self.tspan(self.peek());
                self.recover(
                    code::BAD_FUNCTION_BODY,
                    at,
                    "`=>` needs an inline body or a block",
                )
            }
            _ => self.body(),
        };
        self.loop_depth = saved_loops;
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, lo, self.prev_hi());
        let f = self.alloc(
            NodeKind::FuncDecl,
            flags,
            [name.0, start, len, body.0],
            span,
        );
        self.global_only(f, lo, false)
    }

    /// `param = [ QUAL ] [ type ] NAME [ "=" expr ]`.
    fn param(&mut self) -> NodeId {
        let lo = self.peek().lo;
        let mut qual = 0u32;
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
        if let Some(k) = self.type_end(0) {
            if self.peek_at(k).kind == TokKind::Ident {
                ty = self.type_ref();
            }
        }
        let n = self.peek();
        if n.kind != TokKind::Ident {
            let at = self.tspan(n);
            return self.recover(code::EXPECTED_IDENT, at, "expected a parameter name");
        }
        self.bump();
        let name = self
            .build
            .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
        let mut default = NodeId::NONE;
        if self.peek().kind == TokKind::Eq {
            self.bump();
            default = self.expr(0);
        }
        let span = Span::new(self.file, lo, self.prev_hi());
        self.alloc(NodeKind::Param, 0, [ty.0, name.0, default.0, qual], span)
    }

    /// `type_decl = "type" NAME Newline [ Indent field { field } Dedent ]`.
    pub(super) fn type_decl(&mut self, lo: u32, flags: u8) -> NodeId {
        self.bump();
        let n = self.bump();
        let name = self
            .build
            .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
        let mark = self.build.mark();
        if self.members_open() {
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
                let f = self.field();
                self.build.item(f);
                self.member_end();
            }
        }
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, lo, self.prev_hi());
        let d = self.alloc(NodeKind::TypeDecl, flags, [name.0, start, len, 0], span);
        let local_ok = self.pol.local_type_decl;
        self.global_only(d, lo, local_ok)
    }

    /// `field = [ "varip" ] type NAME [ "=" expr ]`; `var` is pin:01-var-field.
    fn field(&mut self) -> NodeId {
        let lo = self.peek().lo;
        let mut flags = 0u8;
        match self.peek().kind {
            TokKind::Varip => {
                self.bump();
                flags |= nf::VARIP;
            }
            TokKind::Var => {
                if !self.pol.var_field {
                    let at = self.tspan(self.peek());
                    return self.recover(
                        code::EXPECTED_TYPE,
                        at,
                        "`var` is not allowed on a field",
                    );
                }
                // pin:01-var-field accepted: the keyword has no slot in `FieldDecl` (open
                // question in the completion note).
                self.bump();
            }
            _ => {}
        }
        // A field line `import x` is an import path to the lexer (§4.4); the wrapped spelling
        // `import` + `x` on a continuation line is refused the same way (I9).
        let import_word =
            self.peek().kw == SoftKw::Import && self.peek_at(1).kind == TokKind::Ident;
        let typed = match self.type_end(0) {
            Some(k) => self.peek_at(k).kind == TokKind::Ident && !import_word,
            None => false,
        };
        if !typed {
            let at = self.tspan(self.peek());
            return self.recover(code::EXPECTED_TYPE, at, "a field needs a type");
        }
        let ty = self.type_ref();
        let n = self.bump();
        let name = self
            .build
            .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
        let mut default = NodeId::NONE;
        if self.peek().kind == TokKind::Eq {
            self.bump();
            default = self.expr(0);
        }
        let span = Span::new(self.file, lo, self.prev_hi());
        self.alloc(
            NodeKind::FieldDecl,
            flags,
            [ty.0, name.0, default.0, 0],
            span,
        )
    }

    /// `enum_decl = "enum" NAME Newline [ Indent member { member } Dedent ]`.
    pub(super) fn enum_decl(&mut self, lo: u32, flags: u8) -> NodeId {
        self.bump();
        let n = self.bump();
        let name = self
            .build
            .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
        let mark = self.build.mark();
        if self.members_open() {
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
                let m = self.enum_member();
                self.build.item(m);
                self.member_end();
            }
        }
        let (start, len) = self.build.close_list(mark);
        let span = Span::new(self.file, lo, self.prev_hi());
        let d = self.alloc(NodeKind::EnumDecl, flags, [name.0, start, len, 0], span);
        let local_ok = self.pol.local_type_decl;
        self.global_only(d, lo, local_ok)
    }

    /// `member = NAME [ "=" expr ]`.
    fn enum_member(&mut self) -> NodeId {
        let n = self.peek();
        if n.kind != TokKind::Ident {
            let at = self.tspan(n);
            return self.recover(code::EXPECTED_IDENT, at, "expected an enum member name");
        }
        self.bump();
        let name = self
            .build
            .leaf(NodeKind::Name, 0, n.sym.0, 0, self.tspan(n));
        let mut title = NodeId::NONE;
        if self.peek().kind == TokKind::Eq {
            self.bump();
            title = self.expr(0);
        }
        let span = Span::new(self.file, n.lo, self.prev_hi());
        self.alloc(NodeKind::EnumMember, 0, [name.0, title.0, 0, 0], span)
    }

    /// After a `type`/`enum` header: `Newline Indent` opens the member block (consumed);
    /// a header line with trailing tokens is 2000; an empty body is accepted (L9).
    fn members_open(&mut self) -> bool {
        if !matches!(self.peek().kind, TokKind::Newline | TokKind::Eof) {
            let at = self.tspan(self.peek());
            self.recover(code::EXPECTED, at, "expected the end of the header");
            self.skip_to_line_end();
        }
        if self.peek().kind == TokKind::Newline && self.peek_at(1).kind == TokKind::Indent {
            self.bump();
            self.bump();
            return true;
        }
        false
    }

    /// A member line ends at `Newline` (or the block's `Dedent`); else 2000 and skip.
    fn member_end(&mut self) {
        if !matches!(
            self.peek().kind,
            TokKind::Newline | TokKind::Dedent | TokKind::Eof
        ) {
            let at = self.tspan(self.peek());
            self.recover(code::EXPECTED, at, "expected the end of the line");
            self.skip_to_line_end();
        }
    }

    /// `import = "import" SEG "/" SEG "/" SEG [ "as" NAME ]` (segments from the lexer's
    /// import-path mode).
    pub(super) fn import_decl(&mut self) -> NodeId {
        let t = self.bump();
        let user = self.bump();
        let user = self
            .build
            .leaf(NodeKind::Name, 0, user.sym.0, 0, self.tspan(user));
        if self.peek().kind != TokKind::Seg {
            let at = Span::new(self.file, t.lo, self.prev_hi());
            return self.recover(
                code::IMPORT_VERSION_REQUIRED,
                at,
                "an import names user/library/version",
            );
        }
        let l = self.bump();
        let lib = self
            .build
            .leaf(NodeKind::Name, 0, l.sym.0, 0, self.tspan(l));
        if self.peek().kind != TokKind::Seg {
            let at = Span::new(self.file, t.lo, self.prev_hi());
            return self.recover(
                code::IMPORT_VERSION_REQUIRED,
                at,
                "an import must name its library version explicitly",
            );
        }
        let v = self.bump();
        let version = match version_number(&self.src[v.lo as usize..v.hi as usize]) {
            Some(n) => n,
            // pin:01-import-version-form relaxed: a non-integer version has no `u16` slot, so it
            // is recorded as 0 (open question in the completion note).
            None if !self.pol.import_version_int_only => 0,
            None => {
                return self.recover(
                    code::IMPORT_VERSION_BAD,
                    self.tspan(v),
                    "a library version is a decimal integer in 1..=65535",
                )
            }
        };
        let mut alias = NodeId::NONE;
        if self.peek().kw == SoftKw::As {
            self.bump();
            let a = self.peek();
            if a.kind != TokKind::Ident {
                let at = self.tspan(a);
                return self.recover(code::EXPECTED_IDENT, at, "expected an alias after `as`");
            }
            self.bump();
            alias = self
                .build
                .leaf(NodeKind::Name, 0, a.sym.0, 0, self.tspan(a));
        }
        let span = Span::new(self.file, t.lo, self.prev_hi());
        let d = self.alloc(NodeKind::Import, 0, [user.0, lib.0, alias.0, version], span);
        let local_ok = self.pol.local_type_decl;
        self.global_only(d, t.lo, local_ok)
    }

    /// `export = "export" ( func | "method" func | type_decl | enum_decl | "const" typed_tail )`.
    pub(super) fn export_decl(&mut self) -> NodeId {
        let t = self.bump();
        let n = self.peek();
        let after = self.peek_at(1).kind;
        let header_end = matches!(self.peek_at(2).kind, TokKind::Newline | TokKind::Eof);
        let d = match n.kw {
            SoftKw::Type if after == TokKind::Ident && header_end => {
                self.type_decl(t.lo, nf::EXPORT)
            }
            SoftKw::Enum if after == TokKind::Ident && header_end => {
                self.enum_decl(t.lo, nf::EXPORT)
            }
            SoftKw::Method if after == TokKind::Ident => {
                self.bump();
                self.func_decl(t.lo, nf::EXPORT | nf::METHOD)
            }
            SoftKw::Const if self.is_typed_declaration() => {
                if self.dialect == pine_ir::Dialect::V5 {
                    self.diags.at(
                        code::CONSTRUCT_NOT_IN_DIALECT,
                        self.file,
                        t.lo,
                        n.hi,
                        "`export const` is v6-only",
                    );
                }
                self.exported_const(t.lo)
            }
            _ if self.is_function_declaration() => self.func_decl(t.lo, nf::EXPORT),
            _ => {
                let at = self.tspan(n);
                return self.recover(
                    code::EXPECTED,
                    at,
                    "`export` must be followed by a function, method, type, enum or const",
                );
            }
        };
        if self.scope_depth > 0 && !self.pol.local_type_decl {
            return self.global_only(d, t.lo, false);
        }
        d
    }

    /// `"const" typed_tail` after `export`: a `VarDecl` with EXPORT and qualifier `const`.
    fn exported_const(&mut self, lo: u32) -> NodeId {
        let v = self.var_decl(lo, DeclMode::Plain);
        if self.build.kind(v) == NodeKind::VarDecl {
            self.build.set_flags(v, nf::EXPORT);
        }
        v
    }
}

/// A library version: decimal digits only, in `1..=65535`.
fn version_number(d: &[u8]) -> Option<u32> {
    if d.is_empty() || d.len() > 5 {
        return None;
    }
    let mut v = 0u32;
    let mut i = 0usize;
    while i < d.len() {
        if !d[i].is_ascii_digit() {
            return None;
        }
        v = v * 10 + u32::from(d[i] - b'0');
        i += 1;
    }
    if (1..=65535).contains(&v) {
        Some(v)
    } else {
        None
    }
}
