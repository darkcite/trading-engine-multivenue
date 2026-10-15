// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 2: lexical resolution of every use (impl-02 §4.3) — no hoisting of variables, file-wide
//! functions/types/enums, same-scope duplicates, `_`, reassignment targets, reserved names and
//! dotted built-in names. Raises 3, 4, 14, 40, 41, 48.

use pine_ir::{Diag, Dialect, FileId, NodeId, Qual, Span, Sym, SymTable, Ty, TypeQ};
use pine_lang::{Ast, BinOp, Node, UnOp};

use crate::code::{diag, ReasonCode};
use crate::imports::Graph;
use crate::limits::MAX_DECLS;
use crate::limits::MAX_PARAMS;
use crate::pins::{FnGlobalAssign, ForCounterShadow, Pins, ShadowBuiltin};
use crate::scope::{Binding, ScopeKind, Scopes};
use crate::sw::Readings;
use crate::types::{is_collection, type_ref, Named};

/// What pass 2 learned: per file (root first), per node, the binding of each resolved `Ident`.
#[derive(Debug)]
#[expect(dead_code, reason = "read by pass 3's inference (impl-02 T6, §4.4)")]
pub(crate) struct Resolved {
    /// `uses[file][node]`: `Some` for an `Ident` bound to a user declaration.
    pub(crate) uses: Vec<Vec<Option<Binding>>>,
}

/// Run pass 2 over the root and every library that parsed.
pub(crate) fn resolve(
    root: &Ast,
    graph: &Graph,
    syms: &SymTable,
    rd: &Readings,
    diags: &mut Vec<Diag>,
) -> Resolved {
    let mut counters = Counters::default();
    // Declaration indices are `FileId` ascending, then source order (§4.3.1): precomputed so a
    // file may name a later file's types (`alias.T`) before that file is resolved.
    let mut tops = Vec::with_capacity(1 + graph.libs.len());
    let mut bases = Vec::with_capacity(1 + graph.libs.len());
    let mut next = (0u16, 0u16);
    bases.push(next);
    tops.push(top_level(root, &mut next));
    for lib in &graph.libs {
        bases.push(next);
        tops.push(
            lib.as_ref()
                .map_or_else(Vec::new, |a| top_level(a, &mut next)),
        );
    }
    let mut uses = Vec::with_capacity(1 + graph.libs.len());
    let cx = Cx {
        syms,
        tops: &tops,
        pins: rd.pins,
    };
    counters.udts = bases[0].0;
    counters.enums = bases[0].1;
    uses.push(File::run(root, &graph.edges[0], &cx, diags, &mut counters));
    let mut f = 0;
    while f < graph.libs.len() {
        match &graph.libs[f] {
            Some(ast) => uses.push({
                counters.udts = bases[f + 1].0;
                counters.enums = bases[f + 1].1;
                File::run(ast, &graph.edges[f + 1], &cx, diags, &mut counters)
            }),
            None => uses.push(Vec::new()),
        }
        f += 1;
    }
    Resolved { uses }
}

/// The top-level declarations of a file, `(name, export, type)`, in source order: what an
/// importer's `alias.name` may name (§4.2.2, §4.3.4). `next` is the next `(UDT, enum)` index.
fn top_level(ast: &Ast, next: &mut (u16, u16)) -> Vec<(Sym, bool, Option<Named>)> {
    let mut out = Vec::new();
    let Node::Script { body } = ast.node(ast.root()) else {
        return out;
    };
    for &s in body {
        match ast.node(s) {
            Node::TypeDecl { export, name, .. } => {
                out.push((ast.sym(name), export, Some(Named::Udt(next.0))));
                next.0 = next.0.saturating_add(1);
            }
            Node::EnumDecl { export, name, .. } => {
                out.push((ast.sym(name), export, Some(Named::Enum(next.1))));
                next.1 = next.1.saturating_add(1);
            }
            Node::FuncDecl { export, name, .. } | Node::VarDecl { export, name, .. } => {
                out.push((ast.sym(name), export, None));
            }
            _ => {}
        }
    }
    out
}

/// What every file's resolution shares.
struct Cx<'a> {
    syms: &'a SymTable,
    /// `tops[file]`: [`top_level`] of each file (empty for a failed parse).
    tops: &'a [Vec<(Sym, bool, Option<Named>)>],
    pins: Pins,
}

/// One required parameter of a user function signature: its qualifier keyword and written type
/// (`(ty, a, b)` of its `TypeQ`), `None` where unwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParamSig {
    qual: Option<Qual>,
    ty: Option<(Ty, u16, u16)>,
}

/// What the end-of-file constant check needs of one variable.
#[derive(Debug, Clone, Copy)]
struct VarInfo {
    konst: bool,
    init: Option<NodeId>,
    global: bool,
    reassigned: bool,
}

/// Declaration indices, global over the compilation (`FileId` ascending, then source order).
#[derive(Debug, Default)]
struct Counters {
    vars: u32,
    fn_sets: u32,
    udts: u16,
    enums: u16,
}

struct File<'a> {
    ast: &'a Ast,
    syms: &'a SymTable,
    tops: &'a [Vec<(Sym, bool, Option<Named>)>],
    pins: Pins,
    /// User function signatures seen so far: `(name, method, required parameters, span)`.
    sigs: Vec<(Sym, bool, Vec<ParamSig>, Span)>,
    dialect: Dialect,
    diags: &'a mut Vec<Diag>,
    counters: &'a mut Counters,
    scopes: Scopes,
    underscore: Option<Sym>,
    uses: Vec<Option<Binding>>,
    /// Names resolved to built-in values, and declarations shadowing built-in value names
    /// (§4.3.3): their intersection is `3004` at the declaration.
    builtin_uses: Vec<Sym>,
    shadowing: Vec<(Sym, Span)>,
    /// `vars[id - var_base]`: every variable and counter this file declares.
    vars: Vec<VarInfo>,
    var_base: u32,
    /// Expressions that must fold (§4.3.1, checked at the file's end when reassignments are
    /// known): parameter and field defaults (`None`), enum member titles (`Some(Ty::String)`).
    must_fold: Vec<(NodeId, Option<Ty>, &'static str)>,
    /// Uses of a global variable from a library function body (§4.2.2): `(use, var id)`.
    lib_global_uses: Vec<(NodeId, u32)>,
}

/// `pine_spec::lookup(name)` is a built-in value (`close`, `bar_index`, …).
fn is_builtin_value(name: &str) -> bool {
    pine_spec::lookup(name).is_some_and(|e| e.val_lo < e.val_hi)
}

impl<'a> File<'a> {
    fn run(
        ast: &'a Ast,
        imports: &[crate::imports::Import],
        cx: &Cx<'a>,
        diags: &'a mut Vec<Diag>,
        counters: &'a mut Counters,
    ) -> Vec<Option<Binding>> {
        let syms = cx.syms;
        let mut f = File {
            ast,
            syms,
            tops: cx.tops,
            pins: cx.pins,
            sigs: Vec::new(),
            dialect: ast.dialect(),
            diags,
            counters,
            scopes: Scopes::default(),
            underscore: syms.get("_"),
            uses: vec![None; ast.len()],
            builtin_uses: Vec::new(),
            shadowing: Vec::new(),
            vars: Vec::new(),
            var_base: 0,
            must_fold: Vec::new(),
            lib_global_uses: Vec::new(),
        };
        f.scopes.open(ScopeKind::Global);
        for imp in imports {
            f.declare(imp.alias, Binding::Lib { file: imp.file }, imp.span);
        }
        let Node::Script { body } = ast.node(ast.root()) else {
            return f.uses;
        };
        f.declare_file_wide(body);
        for &s in body {
            f.node(s);
        }
        f.scopes.close();
        f.report_shadowing();
        f.check_constants();
        f.uses
    }

    fn text(&self, s: Sym) -> &'a str {
        self.syms.resolve(s)
    }

    fn refuse(&mut self, rc: ReasonCode, at: Span, msg: String) {
        self.diags.push(diag(rc, at, msg));
    }

    /// Declare in the innermost frame; a same-frame duplicate is `3041` with both spans.
    fn declare(&mut self, name: Sym, b: Binding, at: Span) {
        if let Err(prev) = self.scopes.declare(name, b, at, self.underscore) {
            let msg = format!("`{}` is already declared in this scope", self.text(name));
            let mut d = diag(ReasonCode::DuplicateDecl, at, msg);
            d.notes.push((prev, "previous declaration".to_owned()));
            self.diags.push(d);
        }
    }

    /// §4.3.1: functions (not methods), types and enums are visible file-wide.
    fn declare_file_wide(&mut self, body: &[NodeId]) {
        for &s in body {
            let at = self.ast.span(s);
            match self.ast.node(s) {
                Node::FuncDecl {
                    method: false,
                    name,
                    ..
                } => {
                    let n = self.ast.sym(name);
                    self.check_builtin_function_name(n, self.ast.span(name));
                    // One binding per overload set: a second function of one name overloads it.
                    if let Some((Binding::Fns { .. }, _)) = self.scopes.lookup(n) {
                        continue;
                    }
                    let set = self.counters.fn_sets;
                    self.counters.fn_sets += 1;
                    self.declare(n, Binding::Fns { set }, at);
                }
                Node::TypeDecl { name, .. } => {
                    let decl = self.counters.udts;
                    self.counters.udts += 1;
                    self.check_decl_count(decl, "user-defined types", at);
                    self.declare(self.ast.sym(name), Binding::Udt { decl }, at);
                }
                Node::EnumDecl { name, .. } => {
                    let decl = self.counters.enums;
                    self.counters.enums += 1;
                    self.check_decl_count(decl, "enums", at);
                    self.declare(self.ast.sym(name), Binding::Enum { decl }, at);
                }
                _ => {}
            }
        }
    }

    /// A variable-like declaration of `name_node`: the reserved-name checks of §4.3.3, then the
    /// binding.
    fn declare_var(&mut self, name_node: NodeId, b: Binding) {
        let name = self.ast.sym(name_node);
        let at = self.ast.span(name_node);
        let t = self.text(name);
        if Some(name) != self.underscore {
            if t == "na" || pine_spec::lookup(t).is_some_and(|e| e.is_type) {
                self.refuse(
                    ReasonCode::ReservedName,
                    at,
                    format!("`{t}` cannot be declared"),
                );
            } else if is_builtin_value(t) {
                self.shadowing.push((name, at));
            }
        }
        self.declare(name, b, at);
    }

    fn new_var(&mut self, konst: bool, init: Option<NodeId>) -> Binding {
        let id = self.push_var(konst, init);
        Binding::Var { id, konst }
    }

    fn new_counter(&mut self) -> Binding {
        Binding::Counter {
            id: self.push_var(false, None),
        }
    }

    fn push_var(&mut self, konst: bool, init: Option<NodeId>) -> u32 {
        if self.vars.is_empty() {
            self.var_base = self.counters.vars;
        }
        let id = self.counters.vars;
        self.counters.vars += 1;
        let global = self.scopes.depth() == 1;
        self.vars.push(VarInfo {
            konst,
            init,
            global,
            reassigned: false,
        });
        id
    }

    /// A body in its own `Block` frame (§4.3.2 rule 2).
    fn block(&mut self, body: NodeId) {
        self.scopes.open(ScopeKind::Block);
        self.node(body);
        self.scopes.close();
    }

    fn node(&mut self, id: NodeId) {
        let ast = self.ast;
        match ast.node(id) {
            Node::Ident { sym } => self.ident(id, sym),
            Node::Field { .. } => self.field(id),
            Node::Call { callee, args, .. } => {
                self.callee(callee);
                for &a in args {
                    self.node(a);
                }
            }
            Node::NamedArg { value, .. } => self.node(value),
            Node::TypeRef { .. } | Node::Import { .. } | Node::Name { .. } => {}
            Node::VarDecl {
                qual,
                ty,
                name,
                init,
                ..
            } => {
                self.node(init);
                if let Some(t) = ty {
                    let written = self.written_type(t, qual.unwrap_or(Qual::Series));
                    let n = self.text(ast.sym(name));
                    // §4.3.3: a UDT-typed variable named like a namespace (declared types here;
                    // inferred ones in pass 3).
                    if written.is_some_and(|w| w.ty == Ty::Udt) && pine_spec::is_namespace(n) {
                        let msg = format!("a user-defined-type variable cannot be named `{n}`");
                        self.refuse(ReasonCode::ReservedName, ast.span(name), msg);
                    }
                }
                let b = self.new_var(qual == Some(Qual::Const), Some(init));
                self.declare_var(name, b);
            }
            Node::TupleDecl { binders, init } => {
                self.node(init);
                for &b in binders {
                    let v = self.new_var(false, None);
                    self.declare_var(b, v);
                }
            }
            Node::Assign { target, value, .. } => {
                self.node(value);
                self.assign_target(target);
            }
            Node::TupleAssign { targets, value } => {
                self.node(value);
                for &t in targets {
                    self.assign_target(t);
                }
            }
            Node::FuncDecl {
                method,
                name,
                params,
                body,
                ..
            } => {
                self.signature(id, ast.sym(name), method, params);
                self.func(id, params, body);
            }
            Node::TypeDecl { fields, .. } => {
                for &fd in fields {
                    if let Node::FieldDecl {
                        varip,
                        ty,
                        name,
                        default,
                    } = ast.node(fd)
                    {
                        if let Some(t) = self.written_type(ty, Qual::Series) {
                            if is_collection(t) {
                                let msg = format!(
                                    "field `{}` of a user-defined type cannot be a collection",
                                    self.text(ast.sym(name))
                                );
                                self.refuse(ReasonCode::NestedCollection, ast.span(fd), msg);
                            }
                        }
                        if varip {
                            let msg =
                                format!("field `{}` cannot be varip", self.text(ast.sym(name)));
                            self.refuse(ReasonCode::VaripReference, ast.span(fd), msg);
                        }
                        if let Some(d) = default {
                            self.node(d);
                            self.must_fold.push((d, None, "a field default"));
                        }
                    }
                }
            }
            Node::EnumDecl { members, .. } => {
                for &m in members {
                    if let Node::EnumMember { title: Some(t), .. } = ast.node(m) {
                        self.node(t);
                        self.must_fold
                            .push((t, Some(Ty::String), "an enum member title"));
                    }
                }
            }
            Node::If {
                cond,
                then,
                otherwise,
            } => {
                self.node(cond);
                self.block(then);
                if let Some(o) = otherwise {
                    self.block(o);
                }
            }
            Node::Switch { subject, arms } => {
                if let Some(s) = subject {
                    self.node(s);
                }
                for &arm in arms {
                    if let Node::SwitchArm { pattern, body } = ast.node(arm) {
                        if let Some(p) = pattern {
                            self.node(p);
                        }
                        self.block(body);
                    }
                }
            }
            Node::For {
                counter,
                from,
                to,
                step,
                body,
                ..
            } => {
                self.node(from);
                self.node(to);
                if let Some(s) = step {
                    self.node(s);
                }
                self.scopes.open(ScopeKind::Block);
                let c = self.new_counter();
                self.declare(ast.sym(counter), c, ast.span(counter));
                self.loop_body(body);
                self.scopes.close();
            }
            Node::ForIn {
                binders,
                collection,
                body,
            } => {
                self.node(collection);
                self.scopes.open(ScopeKind::Block);
                for &b in binders {
                    let c = self.new_counter();
                    self.declare(ast.sym(b), c, ast.span(b));
                }
                self.loop_body(body);
                self.scopes.close();
            }
            Node::While { cond, body } => {
                self.node(cond);
                self.block(body);
            }
            Node::Once { cond, body } => {
                if let Some(c) = cond {
                    self.node(c);
                }
                self.block(body);
            }
            Node::Block { stmts } => {
                for &s in stmts {
                    self.node(s);
                }
            }
            _ => {
                for c in ast.children(id) {
                    self.node(c);
                }
            }
        }
    }

    /// A function or method declaration: defaults in the enclosing scope, then the body in an
    /// `FnBody` frame with the parameters (§4.3.2 rule 3).
    fn func(&mut self, id: NodeId, params: &[NodeId], body: NodeId) {
        let ast = self.ast;
        if params.len() > MAX_PARAMS {
            let msg = format!("{} parameters (at most {MAX_PARAMS})", params.len());
            self.refuse(ReasonCode::TooManyParams, ast.span(id), msg);
        }
        for &p in params {
            if let Node::Param {
                default: Some(d), ..
            } = ast.node(p)
            {
                self.node(d);
                self.must_fold.push((d, None, "a parameter default"));
            }
        }
        self.scopes.open(ScopeKind::FnBody);
        for (k, &p) in params.iter().enumerate() {
            if let Node::Param { name, .. } = ast.node(p) {
                let index = u8::try_from(k).unwrap_or(u8::MAX);
                self.declare(ast.sym(name), Binding::Param { index }, ast.span(name));
            }
        }
        self.node(body);
        self.scopes.close();
    }

    fn ident(&mut self, id: NodeId, sym: Sym) {
        let at = self.ast.span(id);
        let t = self.text(sym);
        if Some(sym) == self.underscore {
            self.refuse(
                ReasonCode::UnknownName,
                at,
                "`_` is never readable".to_owned(),
            );
            return;
        }
        if let Some((b, _)) = self.scopes.lookup(sym) {
            if let Binding::Var { id: v, .. } = b {
                if self.ast.file() != FileId::ROOT
                    && self.scopes.in_fn()
                    && self.scopes.is_global(sym)
                {
                    self.lib_global_uses.push((id, v));
                }
            }
            self.uses[id.0 as usize] = Some(b);
            return;
        }
        if let Some(e) = pine_spec::lookup(t) {
            self.check_dialect(e, at);
            if is_builtin_value(t) {
                self.builtin_uses.push(sym);
            }
            return;
        }
        let mut d = diag(ReasonCode::UnknownName, at, format!("unknown name `{t}`"));
        if pine_spec::is_namespace(t) {
            d.notes.push((at, "a namespace is not a value".to_owned()));
        }
        self.diags.push(d);
    }

    /// The callee of a call: a user overload set, a built-in function or type, or a dotted name.
    fn callee(&mut self, callee: NodeId) {
        match self.ast.node(callee) {
            Node::Ident { sym } => {
                let at = self.ast.span(callee);
                if let Some((b, _)) = self.scopes.lookup(sym) {
                    self.uses[callee.0 as usize] = Some(b);
                } else if let Some(e) = pine_spec::lookup(self.text(sym)) {
                    self.check_dialect(e, at);
                } else {
                    let msg = format!("unknown function `{}`", self.text(sym));
                    self.refuse(ReasonCode::UnknownName, at, msg);
                }
            }
            _ => self.node(callee),
        }
    }

    /// `a.b…` (§4.3.4): a chain rooted at a user binding resolves its root now (the member waits
    /// for types, pass 3); an unbound root names a built-in path, checked against pine-spec.
    fn field(&mut self, id: NodeId) {
        let ast = self.ast;
        let mut root = id;
        let mut parts: Vec<Sym> = Vec::new();
        loop {
            match ast.node(root) {
                Node::Field { base, member } => {
                    parts.push(ast.sym(member));
                    root = base;
                }
                Node::Ident { sym } => {
                    parts.push(sym);
                    break;
                }
                _ => {
                    self.node(root);
                    return;
                }
            }
        }
        let Node::Ident { sym } = ast.node(root) else {
            unreachable!("loop ends on an Ident")
        };
        if let Some((Binding::Lib { file }, _)) = self.scopes.lookup(sym) {
            self.lib_member(file, parts[parts.len() - 2], ast.span(id));
        }
        if Some(sym) == self.underscore || self.scopes.lookup(sym).is_some() {
            self.ident(root, sym);
            return;
        }
        let mut full = String::new();
        let mut k = parts.len();
        while k > 0 {
            k -= 1;
            full.push_str(self.text(parts[k]));
            if k > 0 {
                full.push('.');
            }
        }
        if let Some(e) = pine_spec::lookup(&full) {
            self.check_dialect(e, ast.span(id));
        } else if !pine_spec::is_namespace(&full) {
            self.refuse(
                ReasonCode::UnknownName,
                ast.span(id),
                format!("unknown name `{full}`"),
            );
        }
    }

    /// `alias.member` of an import (§4.3.4): an exported top-level declaration of that library;
    /// one without `export` is `3038`, none `3003`.
    fn lib_member(&mut self, file: FileId, member: Sym, at: Span) {
        let tops = &self.tops[usize::from(file.0)];
        let name = self.text(member);
        match tops.iter().find(|t| t.0 == member) {
            Some((_, true, _)) => {}
            Some((_, false, _)) => {
                let msg = format!("library item `{name}` is not exported");
                self.refuse(ReasonCode::ImportNotExported, at, msg);
            }
            None => self.refuse(
                ReasonCode::UnknownName,
                at,
                format!("unknown library item `{name}`"),
            ),
        }
    }

    /// A built-in entry without a row in this dialect is `3005` (§4.3.4).
    fn check_dialect(&mut self, e: &pine_spec::Entry, at: Span) {
        if !e.versions.has(self.dialect) {
            let msg = format!(
                "`{}` does not exist in Pine v{}",
                e.full_str(),
                self.dialect as u8
            );
            self.refuse(ReasonCode::NotInDialect, at, msg);
        }
    }

    /// A `for` / `for…in` body: in the counter's frame (`pin:02-for-counter-shadow = Refuse`, a
    /// redeclared counter is `3041`) or in its own frame inside it (`Shadow`).
    fn loop_body(&mut self, body: NodeId) {
        match self.pins.for_counter_shadow {
            ForCounterShadow::Refuse => self.node(body),
            ForCounterShadow::Shadow => self.block(body),
        }
    }

    /// §4.3.3: a user function (not method) named like a built-in function, under
    /// `pin:02-shadow-builtin`.
    fn check_builtin_function_name(&mut self, name: Sym, at: Span) {
        let t = self.text(name);
        if pine_spec::lookup(t).is_none_or(|e| e.ov_lo >= e.ov_hi) {
            return;
        }
        match self.pins.shadow_builtin {
            ShadowBuiltin::Refuse => {
                let msg = format!(
                    "user function `{t}` takes a built-in function's name (pin:02-shadow-builtin)"
                );
                self.refuse(ReasonCode::ReservedName, at, msg);
            }
            ShadowBuiltin::Shadow => {}
        }
    }

    fn var_mut(&mut self, id: u32) -> Option<&mut VarInfo> {
        let k = id.checked_sub(self.var_base)? as usize;
        self.vars.get_mut(k)
    }

    /// End of file (§4.3.1, §4.2.2): which variables are `const` globals (declared `const`, or
    /// never reassigned with an initialiser that folds — inits only name earlier variables, so
    /// one pass in declaration order decides), then every expression that must fold, then every
    /// library function's use of a global.
    fn check_constants(&mut self) {
        let mut is_const = vec![false; self.vars.len()];
        for k in 0..self.vars.len() {
            let v = self.vars[k];
            is_const[k] = v.global
                && (v.konst || !v.reassigned)
                && v.init
                    .is_some_and(|i| self.const_ty(i, &is_const).is_some());
        }
        for k in 0..self.must_fold.len() {
            let (n, want, what) = self.must_fold[k];
            let got = self.const_ty(n, &is_const);
            let ok = match want {
                Some(w) => got == Some(w),
                None => got.is_some(),
            };
            if !ok {
                let msg = format!("{what} must be a compile-time constant");
                self.refuse(ReasonCode::QualifierTooWeak, self.ast.span(n), msg);
            }
        }
        for k in 0..self.lib_global_uses.len() {
            let (n, id) = self.lib_global_uses[k];
            if !is_const[(id - self.var_base) as usize] {
                let at = self.ast.span(n);
                let msg = format!("`{}` is not a const global", self.text(self.ast.sym(n)));
                let mut d = diag(ReasonCode::UnknownName, at, msg);
                d.notes
                    .push((at, "library functions see only `const` globals".to_owned()));
                self.diags.push(d);
            }
        }
    }

    /// The type of `n` when it folds at compile time (§4.17), `Ty::Void` for an untyped `na`;
    /// `None` when it does not. A check only: pass 3 computes the value per Instantiation.
    fn const_ty(&self, n: NodeId, is_const: &[bool]) -> Option<Ty> {
        let ast = self.ast;
        match ast.node(n) {
            Node::IntLit { .. } => Some(Ty::Int),
            Node::FloatLit { .. } => Some(Ty::Float),
            Node::BoolLit { .. } => Some(Ty::Bool),
            Node::StrLit { .. } => Some(Ty::String),
            Node::ColorLit { .. } => Some(Ty::Color),
            Node::Ident { sym } => match self.uses[n.0 as usize] {
                Some(Binding::Var { id, .. }) => {
                    let k = id.checked_sub(self.var_base)? as usize;
                    if *is_const.get(k)? {
                        self.const_ty(self.vars[k].init?, is_const)
                    } else {
                        None
                    }
                }
                Some(_) => None,
                None => self.builtin_const(self.text(sym)),
            },
            Node::Field { .. } => self.field_const(n),
            Node::Unary { op, operand } => {
                let t = self.const_ty(operand, is_const)?;
                match op {
                    UnOp::Not => Some(Ty::Bool),
                    UnOp::Neg | UnOp::Pos => Some(t),
                }
            }
            Node::Binary { op, lhs, rhs } => {
                let (a, b) = (self.const_ty(lhs, is_const)?, self.const_ty(rhs, is_const)?);
                Some(match op {
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne => {
                        Ty::Bool
                    }
                    BinOp::And | BinOp::Or => Ty::Bool,
                    _ if a == Ty::String || b == Ty::String => Ty::String,
                    _ if a == Ty::Float || b == Ty::Float => Ty::Float,
                    BinOp::Div if self.dialect == Dialect::V6 => Ty::Float,
                    _ if a == Ty::Void => b,
                    _ => a,
                })
            }
            Node::Ternary {
                cond,
                then,
                otherwise,
            } => {
                self.const_ty(cond, is_const)?;
                let t = self.const_ty(then, is_const)?;
                let o = self.const_ty(otherwise, is_const)?;
                Some(if t == Ty::Void { o } else { t })
            }
            Node::Call { callee, args, .. } => {
                let name = self.path_text(callee)?;
                let ret = match name.as_str() {
                    "int" | "math.round" => Ty::Int,
                    "float" | "math.sqrt" => Ty::Float,
                    "bool" => Ty::Bool,
                    "string" => Ty::String,
                    "color" | "color.new" | "color.rgb" => Ty::Color,
                    _ => return None,
                };
                for &a in args {
                    let v = match ast.node(a) {
                        Node::NamedArg { value, .. } => value,
                        _ => a,
                    };
                    self.const_ty(v, is_const)?;
                }
                Some(ret)
            }
            _ => None,
        }
    }

    /// A built-in value that is `const` in this dialect (`ValueSrc::Const`, e.g. `color.red`).
    fn builtin_const(&self, full: &str) -> Option<Ty> {
        if full == "na" {
            return Some(Ty::Void);
        }
        let e = pine_spec::lookup(full)?;
        e.values()
            .iter()
            .find(|v| v.versions.has(self.dialect))
            .filter(|v| v.src == pine_spec::ValueSrc::Const && v.ty.qual == Qual::Const)
            .map(|v| v.ty.ty)
    }

    /// `E.member` of a user enum, or a dotted built-in constant.
    fn field_const(&self, n: NodeId) -> Option<Ty> {
        let ast = self.ast;
        let Node::Field { base, .. } = ast.node(n) else {
            return None;
        };
        if let Node::Ident { .. } = ast.node(base) {
            match self.uses[base.0 as usize] {
                Some(Binding::Enum { .. }) => return Some(Ty::Enum),
                Some(_) => return None,
                None => {}
            }
        }
        self.builtin_const(&self.path_text(n)?)
    }

    /// The dotted text of an `Ident` / `Field` chain of unbound names.
    fn path_text(&self, n: NodeId) -> Option<String> {
        match self.ast.node(n) {
            Node::Ident { sym } if self.uses[n.0 as usize].is_none() => {
                Some(self.text(sym).to_owned())
            }
            Node::Field { base, member } => {
                let mut s = self.path_text(base)?;
                s.push('.');
                s.push_str(self.text(self.ast.sym(member)));
                Some(s)
            }
            _ => None,
        }
    }

    /// `3013`: more declarations than `elem::DECL_MASK` can index.
    fn check_decl_count(&mut self, index: u16, what: &str, at: Span) {
        if index > MAX_DECLS {
            let msg = format!("more than {MAX_DECLS} {what}");
            self.refuse(ReasonCode::MonoExplosion, at, msg);
        }
    }

    /// The user type a written path names: an `Ident` bound to a UDT / enum, or `alias.T` naming
    /// an exported type of that library.
    fn named(&self, path: NodeId) -> Result<Named, Diag> {
        let ast = self.ast;
        let at = ast.span(path);
        match ast.node(path) {
            Node::Ident { sym } => match self.scopes.lookup(sym) {
                Some((Binding::Udt { decl }, _)) => Ok(Named::Udt(decl)),
                Some((Binding::Enum { decl }, _)) => Ok(Named::Enum(decl)),
                _ => {
                    let msg = format!("unknown type `{}`", self.text(sym));
                    Err(diag(ReasonCode::UnknownName, at, msg))
                }
            },
            Node::Field { base, member } => {
                let m = ast.sym(member);
                let lib = match ast.node(base) {
                    Node::Ident { sym } => self.scopes.lookup(sym),
                    _ => None,
                };
                let Some((Binding::Lib { file }, _)) = lib else {
                    let msg = format!("unknown type `{}`", self.text(m));
                    return Err(diag(ReasonCode::UnknownName, at, msg));
                };
                match self.tops[usize::from(file.0)].iter().find(|t| t.0 == m) {
                    Some((_, true, Some(n))) => Ok(*n),
                    Some((_, false, Some(_))) => {
                        let msg = format!("library type `{}` is not exported", self.text(m));
                        Err(diag(ReasonCode::ImportNotExported, at, msg))
                    }
                    _ => {
                        let msg = format!("unknown library type `{}`", self.text(m));
                        Err(diag(ReasonCode::UnknownName, at, msg))
                    }
                }
            }
            _ => Err(diag(
                ReasonCode::UnknownName,
                at,
                "expected a type name".to_owned(),
            )),
        }
    }

    /// The `TypeQ` of a written type, its refusal recorded.
    fn written_type(&mut self, node: NodeId, qual: Qual) -> Option<TypeQ> {
        match type_ref(self.ast, self.syms, node, qual, &|p| self.named(p)) {
            Ok(t) => Some(t),
            Err(d) => {
                self.diags.push(d);
                None
            }
        }
    }

    /// §4.3.1 overloads: two user functions (or two methods) of one name with the same
    /// required-parameter types, qualified, position by position, are `3041`.
    fn signature(&mut self, id: NodeId, name: Sym, method: bool, params: &[NodeId]) {
        let ast = self.ast;
        let mut sig = Vec::new();
        let mut known = true;
        for &p in params {
            let Node::Param {
                qual, ty, default, ..
            } = ast.node(p)
            else {
                continue;
            };
            let t = match ty {
                Some(t) => match self.written_type(t, qual.unwrap_or(Qual::Series)) {
                    Some(tq) => Some((tq.ty, tq.a, tq.b)),
                    None => {
                        known = false;
                        None
                    }
                },
                None => None,
            };
            if default.is_none() {
                sig.push(ParamSig { qual, ty: t });
            }
        }
        if !known {
            return;
        }
        let at = ast.span(id);
        if let Some(prev) = self
            .sigs
            .iter()
            .find(|s| s.0 == name && s.1 == method && s.2 == sig)
        {
            let msg = format!(
                "`{}` is already declared with the same required-parameter types",
                self.text(name)
            );
            let mut d = diag(ReasonCode::DuplicateDecl, at, msg);
            d.notes.push((prev.3, "previous declaration".to_owned()));
            self.diags.push(d);
            return;
        }
        self.sigs.push((name, method, sig, at));
    }

    /// A reassignment target (§4.3.2 "Reassignment").
    fn assign_target(&mut self, t: NodeId) {
        let Node::Ident { sym } = self.ast.node(t) else {
            self.node(t); // a field or element store: resolved as an expression
            return;
        };
        let at = self.ast.span(t);
        let name = self.text(sym);
        match self.scopes.lookup(sym) {
            Some((b @ Binding::Var { konst: false, .. }, _)) => {
                if self.scopes.in_fn() && self.scopes.is_global(sym) {
                    match self.pins.fn_global_assign {
                        FnGlobalAssign::Refuse => {
                            let msg = format!(
                                "`{name}` is a global; a function body cannot reassign it \
                                 (pin:02-fn-global-assign)"
                            );
                            self.refuse(ReasonCode::AssignToNonVar, at, msg);
                            return;
                        }
                        FnGlobalAssign::Allow => {}
                    }
                }
                if let Binding::Var { id, .. } = b {
                    if let Some(v) = self.var_mut(id) {
                        v.reassigned = true;
                    }
                }
                self.uses[t.0 as usize] = Some(b);
            }
            Some((b, _)) => {
                let what = match b {
                    Binding::Var { .. } => "a const variable",
                    Binding::Param { .. } => "a parameter",
                    Binding::Counter { .. } => "a loop counter",
                    Binding::Fns { .. } => "a function",
                    Binding::Udt { .. } | Binding::Enum { .. } => "a type",
                    Binding::Lib { .. } => "an import alias",
                };
                self.refuse(
                    ReasonCode::AssignToNonVar,
                    at,
                    format!("`{name}` is {what}"),
                );
            }
            None if Some(sym) != self.underscore && pine_spec::lookup(name).is_some() => {
                self.refuse(
                    ReasonCode::AssignToNonVar,
                    at,
                    format!("`{name}` is a built-in"),
                );
            }
            None => self.refuse(
                ReasonCode::UnknownName,
                at,
                format!("unknown variable `{name}`"),
            ),
        }
    }

    /// §4.3.3: a declaration shadowing a built-in value the file also uses as the built-in.
    fn report_shadowing(&mut self) {
        let mut k = 0;
        while k < self.shadowing.len() {
            let (name, at) = self.shadowing[k];
            if self.builtin_uses.contains(&name) {
                let msg = format!(
                    "`{}` shadows a built-in variable the script also uses as the built-in",
                    self.text(name)
                );
                self.refuse(ReasonCode::ReservedName, at, msg);
            }
            k += 1;
        }
    }
}
