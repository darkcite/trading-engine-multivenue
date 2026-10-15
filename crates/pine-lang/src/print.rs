// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The canonical printer and `structurally_eq` (§3.9, §4.10).

use core::fmt::{Result, Write};

use pine_ir::{NodeId, SymTable};

use crate::ast::{AnnotKind, Annotation, Ast, DeclMode, Node, NodeKind};
use crate::lex::layout::INDENT_UNIT_SPACES;

/// Print `ast` as canonical Pine (§4.10): `//@version=N` first, four spaces per level, one
/// statement per line, every `=>` body as an indented block, every compound operand
/// parenthesised. `parse(print(ast))` is structurally equal to `ast` (I9). Allocates nothing
/// itself; `out` grows as it grows.
pub fn print<W: Write>(ast: &Ast, syms: &SymTable, out: &mut W) -> Result {
    let p = Printer { ast, syms };
    out.write_str("//@version=")?;
    out.write_fmt(format_args!("{}\n", ast.dialect() as u32))?;
    let annos = ast.annotations();
    let mut i = 0usize;
    let mut any = false;
    while i < annos.len() {
        if annos[i].kind != AnnotKind::Version && !in_doc_run(ast, i) {
            p.annotation(&annos[i], 0, out)?;
            any = true;
        }
        i += 1;
    }
    let Node::Script { body } = ast.node(ast.root()) else {
        return Err(core::fmt::Error);
    };
    if any && !body.is_empty() {
        // A blank line keeps script-level annotations out of the first statement's doc run.
        out.write_char('\n')?;
    }
    p.stmts(body, 0, out)?;
    if body.is_empty() {
        return Ok(());
    }
    out.write_char('\n')
}

/// Structural equality (§3.9): kinds, payloads (`Sym`s by resolved bytes), children in order,
/// the dialect and the annotation texts per doc run and at script level. Ignores spans,
/// `StrForm`, and whether a body was written inline. Recursion ≤ `MAX_DEPTH`; no allocation.
#[must_use]
pub fn structurally_eq(a: &Ast, a_syms: &SymTable, b: &Ast, b_syms: &SymTable) -> bool {
    if a.dialect() != b.dialect() {
        return false;
    }
    let (aa, ba) = (a.annotations(), b.annotations());
    // Script-level annotations, in order.
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        while i < aa.len() && (aa[i].kind == AnnotKind::Version || in_doc_run(a, i)) {
            i += 1;
        }
        while j < ba.len() && (ba[j].kind == AnnotKind::Version || in_doc_run(b, j)) {
            j += 1;
        }
        match (i < aa.len(), j < ba.len()) {
            (false, false) => break,
            (true, true) => {
                if !annos_eq(&aa[i..i + 1], a_syms, &ba[j..j + 1], b_syms) {
                    return false;
                }
                i += 1;
                j += 1;
            }
            _ => return false,
        }
    }
    let eq = Eq {
        a,
        a_syms,
        b,
        b_syms,
    };
    eq.node(a.root(), b.root())
}

/// Annotation `i` belongs to some statement's doc run.
fn in_doc_run(ast: &Ast, i: usize) -> bool {
    ast.in_run.get(i).copied().unwrap_or(false)
}

/// Same kinds and texts, pairwise.
fn annos_eq(a: &[Annotation], asy: &SymTable, b: &[Annotation], bsy: &SymTable) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut k = 0usize;
    while k < a.len() {
        if a[k].kind != b[k].kind || asy.resolve_bytes(a[k].text) != bsy.resolve_bytes(b[k].text) {
            return false;
        }
        k += 1;
    }
    true
}

/// The key of an annotation kind.
fn annot_key(k: AnnotKind) -> &'static str {
    match k {
        AnnotKind::Version => "version",
        AnnotKind::Description => "description",
        AnnotKind::Function => "function",
        AnnotKind::Param => "param",
        AnnotKind::Returns => "returns",
        AnnotKind::Type => "type",
        AnnotKind::Enum => "enum",
        AnnotKind::Field => "field",
        AnnotKind::Variable => "variable",
        AnnotKind::StrategyAlertMessage => "strategy_alert_message",
    }
}

/// The printer's read-only context.
struct Printer<'a> {
    ast: &'a Ast,
    syms: &'a SymTable,
}

impl Printer<'_> {
    fn indent<W: Write>(&self, level: u32, out: &mut W) -> Result {
        let mut k = 0u32;
        while k < level * INDENT_UNIT_SPACES {
            out.write_char(' ')?;
            k += 1;
        }
        Ok(())
    }

    fn annotation<W: Write>(&self, a: &Annotation, level: u32, out: &mut W) -> Result {
        self.indent(level, out)?;
        out.write_str("//@")?;
        out.write_str(annot_key(a.kind))?;
        let text = self.syms.resolve(a.text);
        if !text.is_empty() {
            out.write_char(' ')?;
            out.write_str(text)?;
        }
        out.write_char('\n')
    }

    fn sym<W: Write>(&self, id: NodeId, out: &mut W) -> Result {
        match self.ast.node(id) {
            Node::Ident { sym } | Node::Name { sym } => out.write_str(self.syms.resolve(sym)),
            _ => Err(core::fmt::Error),
        }
    }

    /// Statements, one per line at `level`, each under its doc run; no trailing line break.
    fn stmts<W: Write>(&self, list: &[NodeId], level: u32, out: &mut W) -> Result {
        let mut k = 0usize;
        while k < list.len() {
            if k > 0 {
                out.write_char('\n')?;
            }
            let doc = self.ast.doc(list[k]);
            let mut d = 0usize;
            while d < doc.len() {
                self.annotation(&doc[d], level, out)?;
                d += 1;
            }
            self.indent(level, out)?;
            self.stmt(list[k], level, out)?;
            k += 1;
        }
        Ok(())
    }

    /// A `Block` body at `level + 1`, preceded by the header's line break (no trailing one).
    fn block<W: Write>(&self, id: NodeId, level: u32, out: &mut W) -> Result {
        out.write_char('\n')?;
        match self.ast.node(id) {
            Node::Block { stmts } => self.stmts(stmts, level + 1, out),
            _ => Err(core::fmt::Error),
        }
    }

    /// One statement (no line break).
    fn stmt<W: Write>(&self, id: NodeId, level: u32, out: &mut W) -> Result {
        match self.ast.node(id) {
            Node::VarDecl {
                export,
                mode,
                qual,
                ty,
                name,
                init,
            } => {
                if export {
                    out.write_str("export ")?;
                }
                match mode {
                    DeclMode::Plain => {}
                    DeclMode::Var => out.write_str("var ")?,
                    DeclMode::Varip => out.write_str("varip ")?,
                }
                if let Some(q) = qual {
                    out.write_str(qual_text(q))?;
                    out.write_char(' ')?;
                }
                if let Some(t) = ty {
                    self.expr(t, level, out)?;
                    out.write_char(' ')?;
                }
                self.sym(name, out)?;
                out.write_str(" = ")?;
                self.expr(init, level, out)
            }
            Node::TupleDecl { binders, init } => {
                self.list('[', binders, ']', level, out)?;
                out.write_str(" = ")?;
                self.expr(init, level, out)
            }
            Node::TupleAssign { targets, value } => {
                self.list('[', targets, ']', level, out)?;
                out.write_str(" := ")?;
                self.expr(value, level, out)
            }
            Node::Assign { op, target, value } => {
                self.expr(target, level, out)?;
                out.write_char(' ')?;
                out.write_str(op.text())?;
                out.write_char(' ')?;
                self.expr(value, level, out)
            }
            Node::Break => out.write_str("break"),
            Node::Continue => out.write_str("continue"),
            Node::FuncDecl {
                export,
                method,
                name,
                params,
                body,
            } => {
                if export {
                    out.write_str("export ")?;
                }
                if method {
                    out.write_str("method ")?;
                }
                self.sym(name, out)?;
                self.list('(', params, ')', level, out)?;
                out.write_str(" =>")?;
                self.block(body, level, out)
            }
            Node::TypeDecl {
                export,
                name,
                fields,
            } => {
                if export {
                    out.write_str("export ")?;
                }
                out.write_str("type ")?;
                self.sym(name, out)?;
                self.members(fields, level, out)
            }
            Node::EnumDecl {
                export,
                name,
                members,
            } => {
                if export {
                    out.write_str("export ")?;
                }
                out.write_str("enum ")?;
                self.sym(name, out)?;
                self.members(members, level, out)
            }
            Node::Import {
                user,
                lib,
                version,
                alias,
            } => {
                out.write_str("import ")?;
                self.sym(user, out)?;
                out.write_char('/')?;
                self.sym(lib, out)?;
                out.write_fmt(format_args!("/{version}"))?;
                if let Some(a) = alias {
                    out.write_str(" as ")?;
                    self.sym(a, out)?;
                }
                Ok(())
            }
            _ => self.expr(id, level, out),
        }
    }

    /// `type`/`enum` members, one per line at `level + 1`.
    fn members<W: Write>(&self, list: &[NodeId], level: u32, out: &mut W) -> Result {
        let mut k = 0usize;
        while k < list.len() {
            out.write_char('\n')?;
            self.indent(level + 1, out)?;
            match self.ast.node(list[k]) {
                Node::FieldDecl {
                    varip,
                    ty,
                    name,
                    default,
                } => {
                    if varip {
                        out.write_str("varip ")?;
                    }
                    self.expr(ty, level, out)?;
                    out.write_char(' ')?;
                    self.sym(name, out)?;
                    if let Some(d) = default {
                        out.write_str(" = ")?;
                        self.expr(d, level, out)?;
                    }
                }
                Node::EnumMember { name, title } => {
                    self.sym(name, out)?;
                    if let Some(t) = title {
                        out.write_str(" = ")?;
                        self.expr(t, level, out)?;
                    }
                }
                _ => return Err(core::fmt::Error),
            }
            k += 1;
        }
        Ok(())
    }

    /// `open item, item close`.
    fn list<W: Write>(
        &self,
        open: char,
        items: &[NodeId],
        close: char,
        level: u32,
        out: &mut W,
    ) -> Result {
        out.write_char(open)?;
        let mut k = 0usize;
        while k < items.len() {
            if k > 0 {
                out.write_str(", ")?;
            }
            self.expr(items[k], level, out)?;
            k += 1;
        }
        out.write_char(close)
    }

    /// A base of `Field`/`Call`/`Hist`: literals parenthesised (`(1).x`).
    fn base<W: Write>(&self, id: NodeId, level: u32, out: &mut W) -> Result {
        let lit = matches!(
            self.ast.kind(id),
            NodeKind::IntLit
                | NodeKind::FloatLit
                | NodeKind::BoolLit
                | NodeKind::StrLit
                | NodeKind::ColorLit
        );
        if lit {
            out.write_char('(')?;
            self.expr(id, level, out)?;
            return out.write_char(')');
        }
        self.expr(id, level, out)
    }

    /// An expression (or a structure on the right of `=`/`:=`, whose blocks sit at `level + 1`).
    fn expr<W: Write>(&self, id: NodeId, level: u32, out: &mut W) -> Result {
        match self.ast.node(id) {
            Node::Ident { .. } | Node::Name { .. } => self.sym(id, out),
            Node::IntLit { value } => out.write_fmt(format_args!("{value}")),
            Node::FloatLit { value } => out.write_fmt(format_args!("{value:?}")),
            Node::BoolLit { value } => out.write_str(if value { "true" } else { "false" }),
            Node::StrLit { value, .. } => {
                out.write_char('"')?;
                let s = self.syms.resolve(value);
                let b = s.as_bytes();
                let mut run = 0usize;
                let mut k = 0usize;
                while k < b.len() {
                    let esc = match b[k] {
                        b'\\' => "\\\\",
                        b'"' => "\\\"",
                        b'\n' => "\\n",
                        b'\t' => "\\t",
                        _ => "",
                    };
                    if !esc.is_empty() {
                        out.write_str(&s[run..k])?;
                        out.write_str(esc)?;
                        run = k + 1;
                    }
                    k += 1;
                }
                out.write_str(&s[run..])?;
                out.write_char('"')
            }
            Node::ColorLit { rgb, alpha } => match alpha {
                Some(a) => out.write_fmt(format_args!("#{rgb:06X}{a:02X}")),
                None => out.write_fmt(format_args!("#{rgb:06X}")),
            },
            Node::Field { base, member } => {
                self.base(base, level, out)?;
                out.write_char('.')?;
                self.sym(member, out)
            }
            Node::Call {
                callee,
                type_args,
                args,
            } => {
                self.base(callee, level, out)?;
                if !type_args.is_empty() {
                    self.list('<', type_args, '>', level, out)?;
                }
                self.list('(', args, ')', level, out)
            }
            Node::NamedArg { name, value } => {
                self.sym(name, out)?;
                out.write_str(" = ")?;
                self.expr(value, level, out)
            }
            Node::Hist { base, offset } => {
                self.base(base, level, out)?;
                out.write_char('[')?;
                self.expr(offset, level, out)?;
                out.write_char(']')
            }
            Node::Unary { op, operand } => {
                out.write_char('(')?;
                out.write_str(op.text())?;
                out.write_char(if matches!(op, crate::ast::UnOp::Not) {
                    ' '
                } else {
                    '('
                })?;
                self.expr(operand, level, out)?;
                if !matches!(op, crate::ast::UnOp::Not) {
                    out.write_char(')')?;
                }
                out.write_char(')')
            }
            Node::Binary { op, lhs, rhs } => {
                out.write_char('(')?;
                self.expr(lhs, level, out)?;
                out.write_char(' ')?;
                out.write_str(op.text())?;
                out.write_char(' ')?;
                self.expr(rhs, level, out)?;
                out.write_char(')')
            }
            Node::Ternary {
                cond,
                then,
                otherwise,
            } => {
                out.write_char('(')?;
                self.expr(cond, level, out)?;
                out.write_str(" ? ")?;
                self.expr(then, level, out)?;
                out.write_str(" : ")?;
                self.expr(otherwise, level, out)?;
                out.write_char(')')
            }
            Node::Tuple { items } => self.list('[', items, ']', level, out),
            Node::TypeRef {
                path,
                args,
                array_suffix,
            } => {
                self.expr(path, level, out)?;
                if !args.is_empty() {
                    self.list('<', args, '>', level, out)?;
                }
                if array_suffix {
                    out.write_str("[]")?;
                }
                Ok(())
            }
            Node::Param {
                qual,
                ty,
                name,
                default,
            } => {
                if let Some(q) = qual {
                    out.write_str(qual_text(q))?;
                    out.write_char(' ')?;
                }
                if let Some(t) = ty {
                    self.expr(t, level, out)?;
                    out.write_char(' ')?;
                }
                self.sym(name, out)?;
                if let Some(d) = default {
                    out.write_str(" = ")?;
                    self.expr(d, level, out)?;
                }
                Ok(())
            }
            Node::If {
                cond,
                then,
                otherwise,
            } => {
                out.write_str("if ")?;
                self.expr(cond, level, out)?;
                self.block(then, level, out)?;
                if let Some(o) = otherwise {
                    out.write_char('\n')?;
                    self.indent(level, out)?;
                    out.write_str("else")?;
                    if self.ast.kind(o) == NodeKind::If {
                        out.write_char(' ')?;
                        self.expr(o, level, out)?;
                    } else {
                        self.block(o, level, out)?;
                    }
                }
                Ok(())
            }
            Node::Switch { subject, arms } => {
                out.write_str("switch")?;
                if let Some(s) = subject {
                    out.write_char(' ')?;
                    self.expr(s, level, out)?;
                }
                let mut k = 0usize;
                while k < arms.len() {
                    out.write_char('\n')?;
                    self.indent(level + 1, out)?;
                    let Node::SwitchArm { pattern, body } = self.ast.node(arms[k]) else {
                        return Err(core::fmt::Error);
                    };
                    if let Some(p) = pattern {
                        self.expr(p, level + 1, out)?;
                        out.write_char(' ')?;
                    }
                    out.write_str("=>")?;
                    self.block(body, level + 1, out)?;
                    k += 1;
                }
                Ok(())
            }
            Node::For {
                ty,
                counter,
                from,
                to,
                step,
                body,
            } => {
                out.write_str("for ")?;
                if let Some(t) = ty {
                    self.expr(t, level, out)?;
                    out.write_char(' ')?;
                }
                self.sym(counter, out)?;
                out.write_str(" = ")?;
                self.expr(from, level, out)?;
                out.write_str(" to ")?;
                self.expr(to, level, out)?;
                if let Some(s) = step {
                    out.write_str(" by ")?;
                    self.expr(s, level, out)?;
                }
                self.block(body, level, out)
            }
            Node::ForIn {
                binders,
                collection,
                body,
            } => {
                out.write_str("for ")?;
                if binders.len() == 1 {
                    self.sym(binders[0], out)?;
                } else {
                    self.list('[', binders, ']', level, out)?;
                }
                out.write_str(" in ")?;
                self.expr(collection, level, out)?;
                self.block(body, level, out)
            }
            Node::While { cond, body } => {
                out.write_str("while ")?;
                self.expr(cond, level, out)?;
                self.block(body, level, out)
            }
            Node::Once { cond, body } => {
                out.write_str("once")?;
                if let Some(c) = cond {
                    out.write_char(' ')?;
                    self.expr(c, level, out)?;
                }
                self.block(body, level, out)
            }
            _ => Err(core::fmt::Error),
        }
    }
}

/// The source text of a qualifier.
fn qual_text(q: pine_ir::Qual) -> &'static str {
    match q {
        pine_ir::Qual::Const => "const",
        pine_ir::Qual::Input => "input",
        pine_ir::Qual::Simple => "simple",
        pine_ir::Qual::Series => "series",
    }
}

/// The comparison context of `structurally_eq`.
struct Eq<'a> {
    a: &'a Ast,
    a_syms: &'a SymTable,
    b: &'a Ast,
    b_syms: &'a SymTable,
}

impl Eq<'_> {
    fn sym(&self, x: pine_ir::Sym, y: pine_ir::Sym) -> bool {
        self.a_syms.resolve_bytes(x) == self.b_syms.resolve_bytes(y)
    }

    /// Payloads of the typed view (children are compared by `node`).
    fn payload(&self, x: NodeId, y: NodeId) -> bool {
        match (self.a.node(x), self.b.node(y)) {
            (Node::Ident { sym: s }, Node::Ident { sym: t })
            | (Node::Name { sym: s }, Node::Name { sym: t }) => self.sym(s, t),
            (Node::IntLit { value: v }, Node::IntLit { value: w }) => v == w,
            (Node::FloatLit { value: v }, Node::FloatLit { value: w }) => {
                v.to_bits() == w.to_bits()
            }
            (Node::BoolLit { value: v }, Node::BoolLit { value: w }) => v == w,
            (Node::StrLit { value: v, .. }, Node::StrLit { value: w, .. }) => self.sym(v, w),
            (Node::ColorLit { rgb: r, alpha: a }, Node::ColorLit { rgb: s, alpha: b }) => {
                r == s && a == b
            }
            (Node::Unary { op: o, .. }, Node::Unary { op: p, .. }) => o == p,
            (Node::Binary { op: o, .. }, Node::Binary { op: p, .. }) => o == p,
            (Node::Assign { op: o, .. }, Node::Assign { op: p, .. }) => o == p,
            (Node::Call { type_args: t, .. }, Node::Call { type_args: u, .. }) => {
                t.len() == u.len()
            }
            (Node::ForIn { binders: t, .. }, Node::ForIn { binders: u, .. }) => t.len() == u.len(),
            (
                Node::VarDecl {
                    export: e,
                    mode: m,
                    qual: q,
                    ..
                },
                Node::VarDecl {
                    export: f,
                    mode: n,
                    qual: r,
                    ..
                },
            ) => e == f && m == n && q == r,
            (
                Node::FuncDecl {
                    export: e,
                    method: m,
                    ..
                },
                Node::FuncDecl {
                    export: f,
                    method: n,
                    ..
                },
            ) => e == f && m == n,
            (Node::Param { qual: q, .. }, Node::Param { qual: r, .. }) => q == r,
            (Node::TypeDecl { export: e, .. }, Node::TypeDecl { export: f, .. })
            | (Node::EnumDecl { export: e, .. }, Node::EnumDecl { export: f, .. }) => e == f,
            (Node::FieldDecl { varip: v, .. }, Node::FieldDecl { varip: w, .. }) => v == w,
            (Node::Import { version: v, .. }, Node::Import { version: w, .. }) => v == w,
            (
                Node::TypeRef {
                    array_suffix: s,
                    args: a,
                    ..
                },
                Node::TypeRef {
                    array_suffix: t,
                    args: b,
                    ..
                },
            ) => s == t && a.len() == b.len(),
            _ => true,
        }
    }

    fn node(&self, x: NodeId, y: NodeId) -> bool {
        if self.a.kind(x) != self.b.kind(y) || !self.payload(x, y) {
            return false;
        }
        if !annos_eq(self.a.doc(x), self.a_syms, self.b.doc(y), self.b_syms) {
            return false;
        }
        let n = self.a.child_count(x);
        if n != self.b.child_count(y) {
            return false;
        }
        let mut k = 0u32;
        while k < n {
            if !self.node(self.a.child(x, k), self.b.child(y, k)) {
                return false;
            }
            k += 1;
        }
        true
    }
}
