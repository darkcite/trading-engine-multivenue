// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §4.8.2: a lexical resolver whose only job is alpha-invariance. It is **not** a claim
//! about Pine's scoping rules; it is deterministic, depends only on structure and name equality,
//! and never treats a free name as bound or the reverse.
//!
//! - Ordinals are assigned to declarations in walk order (pre-order, operands left to right),
//!   over the whole tree, before any resolution.
//! - Scopes: the root, each function (parameters and body), each `Block`; a `for` counter and
//!   the `for…in` binders live in a scope around the loop body.
//! - Functions, types, enums and import aliases are hoisted; everything else is visible from the
//!   statement after its declaring statement (a variable never sees itself in its own
//!   initialiser).
//! - The traversal recurses over the `Ok` tree, whose height `pine_lang` bounds at `MAX_DEPTH`
//!   (impl-01 I7: "Consumers may recurse over an `Ast` without further bounds").

use pine_ir::{Dialect, NodeId, Sym, SymTable};

use crate::ast_view::{AssignOp, Ast, Node, Visit};

/// "No declaration" in the per-node tables.
pub(crate) const NONE: u32 = u32::MAX;

/// What a declaration declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeclKind {
    /// A variable (`VarDecl`, a tuple binder, a loop binder).
    Var,
    /// A function parameter.
    Param,
    /// A function or method.
    Func,
    /// A user type.
    Type,
    /// A user enum.
    Enum,
    /// An import alias.
    Import,
}

/// A receiver's type namespace (§4.8.2 "Receiver facts").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recv {
    /// Not decidable without types.
    Unknown,
    /// A pine-spec type namespace (`array`, `map`, `chart.point`, …).
    Ns(String),
    /// A type or enum the script declares (its declaration index).
    User(u32),
}

/// One declaration.
#[derive(Debug, Clone)]
pub(crate) struct Decl {
    pub(crate) sym: Sym,
    pub(crate) kind: DeclKind,
    pub(crate) ord: u32,
    pub(crate) reads: u32,
    pub(crate) writes: u32,
    pub(crate) recv: Recv,
    /// For an import alias: the index into the script's imports, in source order.
    pub(crate) import: Option<u32>,
    /// The declaring statement (`VarDecl`, `Param`, `FuncDecl`, …, or the `Import` node).
    pub(crate) site: NodeId,
}

/// The resolver's result.
#[derive(Debug, Clone)]
pub(crate) struct Binding {
    pub(crate) decls: Vec<Decl>,
    /// Per node: the declaration a declaring `Name` (or an `Import` node) introduces.
    pub(crate) decl_at: Vec<u32>,
    /// Per node: the declaration an `Ident` resolves to (`NONE` = free).
    pub(crate) use_of: Vec<u32>,
    /// User methods: `(name, the base of the first parameter's type)`.
    pub(crate) methods: Vec<(Sym, Recv)>,
}

impl Binding {
    /// The declaration an `Ident` resolves to.
    pub(crate) fn resolved(&self, id: NodeId) -> Option<&Decl> {
        let d = *self.use_of.get(id.0 as usize)?;
        if d == NONE {
            None
        } else {
            self.decls.get(d as usize)
        }
    }
    /// The declaration index an `Ident` resolves to.
    pub(crate) fn resolved_idx(&self, id: NodeId) -> Option<u32> {
        let d = *self.use_of.get(id.0 as usize)?;
        (d != NONE).then_some(d)
    }
    /// The ordinal of a declaring `Name` or an `Import` node (0 when it declares nothing).
    pub(crate) fn ord_at(&self, id: NodeId) -> u32 {
        match self.decl_at.get(id.0 as usize) {
            Some(&d) if d != NONE => self.decls[d as usize].ord,
            _ => 0,
        }
    }
    /// The declaration a declaring `Name` introduces.
    pub(crate) fn decl_idx_at(&self, id: NodeId) -> Option<u32> {
        match self.decl_at.get(id.0 as usize) {
            Some(&d) if d != NONE => Some(d),
            _ => None,
        }
    }
}

struct B<'a> {
    ast: &'a Ast,
    syms: &'a SymTable,
    dialect: Dialect,
    out: Binding,
    frames: Vec<Vec<(Sym, u32)>>,
    imports_seen: u32,
}

/// Run the resolver over an `Ok` tree.
pub(crate) fn bind(ast: &Ast, syms: &SymTable) -> Binding {
    let n = ast.len();
    let mut b = B {
        ast,
        syms,
        dialect: ast.dialect(),
        out: Binding {
            decls: Vec::new(),
            decl_at: vec![NONE; n],
            use_of: vec![NONE; n],
            methods: Vec::new(),
        },
        frames: Vec::new(),
        imports_seen: 0,
    };
    b.ordinals();
    let root = ast.root();
    if let Node::Script { body } = ast.node(root) {
        b.frames.push(Vec::new());
        b.hoist(body);
        for &s in body {
            b.stmt(s);
        }
        b.frames.pop();
    }
    b.out
}

impl B<'_> {
    fn name_sym(&self, id: NodeId) -> Sym {
        match self.ast.node(id) {
            Node::Name { sym } | Node::Ident { sym } => sym,
            _ => Sym::NONE,
        }
    }

    fn new_decl(&mut self, at: NodeId, site: NodeId, sym: Sym, kind: DeclKind) {
        let ord = self.out.decls.len() as u32;
        self.out.decls.push(Decl {
            sym,
            kind,
            ord,
            reads: 0,
            writes: 0,
            recv: Recv::Unknown,
            import: None,
            site,
        });
        self.out.decl_at[at.0 as usize] = ord;
    }

    /// Pass 1: one ordinal per declaration, in walk order. A declaration's index equals its
    /// ordinal.
    fn ordinals(&mut self) {
        let ast = self.ast;
        let mut declaring = vec![None::<(DeclKind, NodeId)>; ast.len()];
        for v in ast.walk(ast.root()) {
            let Visit::Enter(id) = v else {
                continue;
            };
            if let Some((k, site)) = declaring[id.0 as usize] {
                let s = self.name_sym(id);
                self.new_decl(id, site, s, k);
                continue;
            }
            match ast.node(id) {
                Node::VarDecl { name, .. } => {
                    declaring[name.0 as usize] = Some((DeclKind::Var, id))
                }
                Node::TupleDecl { binders, .. } => {
                    for &x in binders {
                        declaring[x.0 as usize] = Some((DeclKind::Var, id));
                    }
                }
                Node::Param { name, .. } => {
                    declaring[name.0 as usize] = Some((DeclKind::Param, id))
                }
                Node::For { counter, .. } => {
                    declaring[counter.0 as usize] = Some((DeclKind::Var, id));
                }
                Node::ForIn { binders, .. } => {
                    for &x in binders {
                        declaring[x.0 as usize] = Some((DeclKind::Var, id));
                    }
                }
                Node::FuncDecl { name, .. } => {
                    declaring[name.0 as usize] = Some((DeclKind::Func, id))
                }
                Node::TypeDecl { name, .. } => {
                    declaring[name.0 as usize] = Some((DeclKind::Type, id))
                }
                Node::EnumDecl { name, .. } => {
                    declaring[name.0 as usize] = Some((DeclKind::Enum, id))
                }
                Node::Import { lib, alias, .. } => {
                    // The alias (explicit or implicit) is declared at the Import node itself.
                    let s = match alias {
                        Some(a) => self.name_sym(a),
                        None => self.name_sym(lib),
                    };
                    self.new_decl(id, id, s, DeclKind::Import);
                    let idx = self.out.decls.len() - 1;
                    self.out.decls[idx].import = Some(self.imports_seen);
                    self.imports_seen += 1;
                }
                _ => {}
            }
        }
    }

    fn add(&mut self, decl: u32) {
        let s = self.out.decls[decl as usize].sym;
        if let Some(f) = self.frames.last_mut() {
            f.push((s, decl));
        }
    }

    fn add_at(&mut self, name: NodeId) {
        if let Some(d) = self.out.decl_idx_at(name) {
            self.add(d);
        }
    }

    fn lookup(&self, s: Sym) -> Option<u32> {
        for f in self.frames.iter().rev() {
            for &(x, d) in f.iter().rev() {
                if x == s {
                    return Some(d);
                }
            }
        }
        None
    }

    fn hoist(&mut self, stmts: &[NodeId]) {
        for &s in stmts {
            match self.ast.node(s) {
                Node::FuncDecl { name, .. } | Node::TypeDecl { name, .. } => self.add_at(name),
                Node::EnumDecl { name, .. } => self.add_at(name),
                Node::Import { .. } => {
                    if let Some(d) = self.out.decl_idx_at(s) {
                        self.add(d);
                    }
                }
                _ => {}
            }
        }
    }

    fn block(&mut self, id: NodeId) {
        match self.ast.node(id) {
            Node::Block { stmts } => {
                self.frames.push(Vec::new());
                self.hoist(stmts);
                for &s in stmts {
                    self.stmt(s);
                }
                self.frames.pop();
            }
            _ => self.stmt(id),
        }
    }

    fn ident(&mut self, id: NodeId, write: bool) {
        let s = self.name_sym(id);
        if let Some(d) = self.lookup(s) {
            self.out.use_of[id.0 as usize] = d;
            let decl = &mut self.out.decls[d as usize];
            if write {
                decl.writes += 1;
            } else {
                decl.reads += 1;
            }
        }
    }

    /// Resolve the root of a `TypeRef` path without counting a read.
    fn typeref(&mut self, id: NodeId) {
        if let Node::TypeRef { path, args, .. } = self.ast.node(id) {
            let root = self.chain_root(path);
            if let Some(r) = root {
                let s = self.name_sym(r);
                if let Some(d) = self.lookup(s) {
                    self.out.use_of[r.0 as usize] = d;
                }
            }
            for &a in args {
                self.typeref(a);
            }
        }
    }

    /// The root `Ident` of an `Ident`/`Field` chain, if the chain is one.
    fn chain_root(&self, mut id: NodeId) -> Option<NodeId> {
        loop {
            match self.ast.node(id) {
                Node::Ident { .. } => return Some(id),
                Node::Field { base, .. } => id = base,
                _ => return None,
            }
        }
    }

    /// The dotted text of a free chain (`ta.sma`), or `None` when it is not a chain.
    fn chain_text(&self, id: NodeId) -> Option<String> {
        match self.ast.node(id) {
            Node::Ident { sym } => Some(self.syms.resolve(sym).to_string()),
            Node::Field { base, member } => {
                let mut t = self.chain_text(base)?;
                t.push('.');
                t.push_str(self.syms.resolve(self.name_sym(member)));
                Some(t)
            }
            _ => None,
        }
    }

    /// §4.8.2 "Receiver facts" for a declaration with type `ty` and initialiser `init`.
    fn recv_of(&self, ty: Option<NodeId>, init: Option<NodeId>) -> Recv {
        if let Some(t) = ty {
            if let Node::TypeRef {
                path, array_suffix, ..
            } = self.ast.node(t)
            {
                if array_suffix {
                    return Recv::Ns("array".to_string());
                }
                return self.recv_of_path(path);
            }
        }
        if let Some(i) = init {
            if let Node::Call { callee, .. } = self.ast.node(i) {
                if let Some(root) = self.chain_root(callee) {
                    if let Some(d) = self.lookup(self.name_sym(root)) {
                        let k = self.out.decls[d as usize].kind;
                        if matches!(k, DeclKind::Type | DeclKind::Enum) {
                            return Recv::User(d);
                        }
                        return Recv::Unknown;
                    }
                    if let Some(text) = self.chain_text(callee) {
                        if let Some((q, last)) = text.rsplit_once('.') {
                            let typed = crate::spec_view::lookup(q)
                                .is_some_and(|e| e.is_type() && e.in_dialect(self.dialect));
                            // D-H1: a type namespace's constructor (`array.new_float`,
                            // `map.new`, `array.from`, `chart.point.from_index`, …).
                            let ctor = last.starts_with("new")
                                || matches!(
                                    last,
                                    "from" | "copy" | "from_index" | "from_time" | "now"
                                );
                            if typed || (ctor && crate::spec_view::is_namespace(q)) {
                                return Recv::Ns(q.to_string());
                            }
                        }
                    }
                }
            }
        }
        Recv::Unknown
    }

    fn recv_of_path(&self, path: NodeId) -> Recv {
        let Some(root) = self.chain_root(path) else {
            return Recv::Unknown;
        };
        if let Some(d) = self.lookup(self.name_sym(root)) {
            return match self.out.decls[d as usize].kind {
                DeclKind::Type | DeclKind::Enum => Recv::User(d),
                _ => Recv::Unknown,
            };
        }
        match self.chain_text(path) {
            Some(t) => Recv::Ns(t),
            None => Recv::Unknown,
        }
    }

    fn set_recv(&mut self, name: NodeId, r: Recv) {
        if let Some(d) = self.out.decl_idx_at(name) {
            self.out.decls[d as usize].recv = r;
        }
    }

    fn stmt(&mut self, id: NodeId) {
        match self.ast.node(id) {
            Node::VarDecl { ty, name, init, .. } => {
                if let Some(t) = ty {
                    self.typeref(t);
                }
                self.expr(init);
                let r = self.recv_of(ty, Some(init));
                self.set_recv(name, r);
                self.add_at(name);
            }
            Node::TupleDecl { binders, init } => {
                self.expr(init);
                for &x in binders {
                    self.add_at(x);
                }
            }
            Node::TupleAssign { targets, value } => {
                self.expr(value);
                for &t in targets {
                    match self.ast.node(t) {
                        Node::Ident { .. } => self.ident(t, true),
                        _ => self.expr(t),
                    }
                }
            }
            Node::Assign { op, target, value } => {
                self.expr(value);
                match self.ast.node(target) {
                    Node::Ident { .. } => self.ident(target, op == AssignOp::Reassign),
                    _ => self.expr(target),
                }
            }
            Node::FuncDecl {
                method,
                params,
                body,
                name,
                ..
            } => {
                self.frames.push(Vec::new());
                let mut first: Option<Recv> = None;
                for (k, &p) in params.iter().enumerate() {
                    if let Node::Param {
                        ty,
                        name: pn,
                        default,
                        ..
                    } = self.ast.node(p)
                    {
                        if let Some(t) = ty {
                            self.typeref(t);
                        }
                        if let Some(d) = default {
                            self.expr(d);
                        }
                        let r = self.recv_of(ty, None);
                        if k == 0 {
                            first = Some(r.clone());
                        }
                        self.set_recv(pn, r);
                        self.add_at(pn);
                    }
                }
                if method {
                    let s = self.name_sym(name);
                    self.out.methods.push((s, first.unwrap_or(Recv::Unknown)));
                }
                self.block(body);
                self.frames.pop();
            }
            Node::TypeDecl { fields, .. } => {
                for &f in fields {
                    if let Node::FieldDecl { ty, default, .. } = self.ast.node(f) {
                        self.typeref(ty);
                        if let Some(d) = default {
                            self.expr(d);
                        }
                    }
                }
            }
            Node::EnumDecl { members, .. } => {
                for &m in members {
                    if let Node::EnumMember { title: Some(t), .. } = self.ast.node(m) {
                        self.expr(t);
                    }
                }
            }
            Node::Import { .. } | Node::Break | Node::Continue | Node::Error => {}
            Node::If {
                cond,
                then,
                otherwise,
            } => {
                self.expr(cond);
                self.block(then);
                if let Some(o) = otherwise {
                    self.block(o);
                }
            }
            Node::Switch { subject, arms } => {
                if let Some(s) = subject {
                    self.expr(s);
                }
                for &a in arms {
                    if let Node::SwitchArm { pattern, body } = self.ast.node(a) {
                        if let Some(p) = pattern {
                            self.expr(p);
                        }
                        self.block(body);
                    }
                }
            }
            Node::For {
                ty,
                counter,
                from,
                to,
                step,
                body,
            } => {
                if let Some(t) = ty {
                    self.typeref(t);
                }
                self.expr(from);
                self.expr(to);
                if let Some(s) = step {
                    self.expr(s);
                }
                self.frames.push(Vec::new());
                self.add_at(counter);
                self.block(body);
                self.frames.pop();
            }
            Node::ForIn {
                binders,
                collection,
                body,
            } => {
                self.expr(collection);
                self.frames.push(Vec::new());
                for &x in binders {
                    self.add_at(x);
                }
                self.block(body);
                self.frames.pop();
            }
            Node::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            Node::Once { cond, body } => {
                if let Some(c) = cond {
                    self.expr(c);
                }
                self.block(body);
            }
            Node::Block { .. } => self.block(id),
            _ => self.expr(id),
        }
    }

    fn expr(&mut self, id: NodeId) {
        match self.ast.node(id) {
            Node::Ident { .. } => self.ident(id, false),
            Node::Field { base, .. } => self.expr(base),
            Node::Call {
                callee,
                type_args,
                args,
            } => {
                self.expr(callee);
                for &t in type_args {
                    self.typeref(t);
                }
                for &a in args {
                    self.expr(a);
                }
            }
            Node::NamedArg { value, .. } => self.expr(value),
            Node::Hist { base, offset } => {
                self.expr(base);
                self.expr(offset);
            }
            Node::Unary { operand, .. } => self.expr(operand),
            Node::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Node::Ternary {
                cond,
                then,
                otherwise,
            } => {
                self.expr(cond);
                self.expr(then);
                self.expr(otherwise);
            }
            Node::Tuple { items } => {
                for &x in items {
                    self.expr(x);
                }
            }
            Node::TypeRef { .. } => self.typeref(id),
            Node::IntLit { .. }
            | Node::FloatLit { .. }
            | Node::BoolLit { .. }
            | Node::StrLit { .. }
            | Node::ColorLit { .. }
            | Node::Name { .. }
            | Node::Error => {}
            _ => self.stmt(id),
        }
    }
}
