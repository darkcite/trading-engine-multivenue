// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §4.8: one source, one parse, every fact. The one `pine_lang::parse` call of the crate
//! lives here (I24). Built-in references follow §4.8.3: only the outermost `Ident`/`Field` chain
//! in a value, callee or type position is resolved, so nothing is counted twice.

use std::collections::BTreeMap;

use pine_ir::{DiagCode, Dialect, FileId, NodeId, ScriptKind, SymTable};

use crate::ast_view::{Ast, Node};
use crate::bind::{Binding, DeclKind, Recv};
use crate::spec_view::{self, SEntry};

/// One `import <user>/<lib>/<ver> [as <alias>]`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LibKey {
    /// Publishing user.
    pub user: String,
    /// Library name.
    pub lib: String,
    /// Version. impl-01 v4 parses it as a `u16` in `1..=65535`.
    pub ver: u32,
}

impl LibKey {
    /// `"user/lib/ver"`.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}/{}/{}", self.user, self.lib, self.ver)
    }
}

/// References to one pine-spec entry from one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinUse {
    /// The entry's dotted full name, borrowed from pine-spec's static name table.
    pub full: &'static str,
    /// Sites where it is called.
    pub calls: u32,
    /// Sites where it is read as a value (`close`, `strategy.long`).
    pub values: u32,
    /// Sites where it names a type (`array<float>`, `line`).
    pub types: u32,
}

/// `plot` at top level, `plot` elsewhere, `hline` (§4.8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlotCounts {
    /// Plot calls that are a top-level statement or a top-level `VarDecl`'s initialiser.
    pub plot_top: u32,
    /// Every other plot call.
    pub plot_nested: u32,
    /// `hline(...)` calls.
    pub hline: u32,
}

/// Everything one parse of one source yields.
#[derive(Debug, Clone)]
pub struct Scan {
    /// The caller-owned interner the parse used (contract §3.8.1). Every `Sym` below indexes it.
    pub syms: SymTable,
    /// `Ast::dialect()`. `None` when the parse failed.
    pub dialect: Option<Dialect>,
    /// The declaration statement's kind; `None` when it is absent or repeated, or the parse failed.
    pub kind: Option<ScriptKind>,
    /// The codes of `pine_lang::parse`'s `Err` diagnostics, ascending and unique. Empty on success.
    pub parse_codes: Vec<DiagCode>,
    /// Imports, in source order.
    pub imports: Vec<LibKey>,
    /// pine-spec entries referenced, ascending by `full`.
    pub uses: Vec<BuiltinUse>,
    /// Names that no entry covers in this dialect (7102 detail), ascending, unique.
    pub unknown: Vec<String>,
    /// Methods whose receiver type is unknown, as `?.<name>`, ascending, unique.
    pub methods_unresolved: Vec<String>,
    /// Every `input*()` call site, in source order.
    pub inputs: Vec<crate::inputs::CorpusInput>,
    /// Plot capture counts.
    pub plots: PlotCounts,
    /// SHA-256 of the Exact canonical stream (§4.9). `None` when the parse failed.
    pub h_exact: Option<[u8; 32]>,
    /// SHA-256 of the Fork canonical stream. `None` when the parse failed.
    pub h_fork: Option<[u8; 32]>,
}

/// Parse `src` with a fresh caller-owned `SymTable`: **the crate's one parse call** (I24).
pub(crate) fn parse_src(src: &str) -> (SymTable, Result<Ast, Vec<DiagCode>>) {
    let mut syms = SymTable::new();
    let r = match pine_lang::parse(src, FileId::ROOT, &mut syms) {
        Ok(ast) => Ok(ast),
        Err(diags) => {
            let errs: Vec<DiagCode> = diags
                .iter()
                .filter(|d| d.sev == pine_ir::Severity::Error)
                .map(|d| d.code)
                .collect();
            let mut codes = if errs.is_empty() {
                diags.iter().map(|d| d.code).collect()
            } else {
                errs
            };
            codes.sort_unstable();
            codes.dedup();
            Err(codes)
        }
    };
    (syms, r)
}

/// The facts a resolved tree offers to `scan`, `inputs` and `canon`.
pub(crate) struct Ctx<'a> {
    pub(crate) ast: &'a Ast,
    pub(crate) syms: &'a SymTable,
    pub(crate) bind: &'a Binding,
    pub(crate) dialect: Dialect,
}

/// What a call's callee is (§4.8.3).
#[derive(Debug, Clone)]
pub(crate) enum Callee {
    /// A pine-spec entry. `recv` is the receiver expression of a method call (written in
    /// function form, receiver first, §4.9.2).
    Spec { entry: SEntry, recv: Option<NodeId> },
    /// A user function (its declaration index).
    UserFn(u32),
    /// A library member (`u.fn`).
    Library,
    /// A user method overriding a built-in: user code, never counted.
    UserMethod,
    /// A method whose receiver type is unknown: `?.m`.
    Unresolved(String),
    /// A name no entry covers in this dialect (7102 detail).
    Unknown(String),
    /// Anything else: user code (a type member, a field, a chained call).
    Other,
}

impl Ctx<'_> {
    pub(crate) fn text(&self, s: pine_ir::Sym) -> &str {
        self.syms.resolve(s)
    }
    pub(crate) fn name_text(&self, id: NodeId) -> &str {
        match self.ast.node(id) {
            Node::Name { sym } | Node::Ident { sym } => self.syms.resolve(sym),
            _ => "",
        }
    }
    /// The root `Ident` of an `Ident`/`Field` chain, and the chain's length in segments.
    pub(crate) fn chain(&self, mut id: NodeId) -> Option<(NodeId, u32)> {
        let mut n = 1u32;
        loop {
            match self.ast.node(id) {
                Node::Ident { .. } => return Some((id, n)),
                Node::Field { base, .. } => {
                    id = base;
                    n += 1;
                }
                _ => return None,
            }
        }
    }
    /// The dotted text of a chain.
    pub(crate) fn chain_text(&self, id: NodeId) -> String {
        match self.ast.node(id) {
            Node::Ident { sym } => self.syms.resolve(sym).to_string(),
            Node::Field { base, member } => {
                let mut t = self.chain_text(base);
                t.push('.');
                t.push_str(self.name_text(member));
                t
            }
            _ => String::new(),
        }
    }
    /// The member path of a chain below its root (`u.Type.new` → `Type.new`).
    pub(crate) fn member_path(&self, id: NodeId) -> String {
        let t = self.chain_text(id);
        match t.split_once('.') {
            Some((_, rest)) => rest.to_string(),
            None => String::new(),
        }
    }
    /// The entry of a free dotted path, when it exists in this dialect.
    pub(crate) fn spec_hit(&self, path: &str) -> Option<SEntry> {
        spec_view::lookup(path).filter(|e| e.in_dialect(self.dialect))
    }

    /// Classify a callee (§4.8.3).
    pub(crate) fn classify(&self, callee: NodeId) -> Callee {
        if let Some((root, len)) = self.chain(callee) {
            if let Some(d) = self.bind.resolved(root) {
                return match d.kind {
                    DeclKind::Import => Callee::Library,
                    DeclKind::Func if len == 1 => {
                        Callee::UserFn(self.bind.resolved_idx(root).unwrap_or(0))
                    }
                    DeclKind::Var | DeclKind::Param if len == 2 => {
                        let Node::Field { member, .. } = self.ast.node(callee) else {
                            return Callee::Other;
                        };
                        let m = self.name_text(member).to_string();
                        self.method(&d.recv, &m, root)
                    }
                    DeclKind::Var | DeclKind::Param if len > 2 => {
                        let Node::Field { member, .. } = self.ast.node(callee) else {
                            return Callee::Other;
                        };
                        Callee::Unresolved(format!("?.{}", self.name_text(member)))
                    }
                    _ => Callee::Other,
                };
            }
            let p = self.chain_text(callee);
            return match self.spec_hit(&p) {
                Some(e) => Callee::Spec {
                    entry: e,
                    recv: None,
                },
                None => Callee::Unknown(p),
            };
        }
        if let Node::Field { member, .. } = self.ast.node(callee) {
            return Callee::Unresolved(format!("?.{}", self.name_text(member)));
        }
        Callee::Other
    }

    fn method(&self, recv: &Recv, m: &str, root: NodeId) -> Callee {
        // A user method on the receiver's type overrides a built-in (methods page).
        let user = self
            .bind
            .methods
            .iter()
            .any(|(s, r)| self.text(*s) == m && *r == *recv && *r != Recv::Unknown);
        if user {
            return Callee::UserMethod;
        }
        match recv {
            Recv::Ns(ns) => match spec_view::lookup_parts(ns, m) {
                Some(e) if e.in_dialect(self.dialect) => Callee::Spec {
                    entry: e,
                    recv: Some(root),
                },
                _ => Callee::Unknown(format!("{ns}.{m}")),
            },
            Recv::User(_) | Recv::Unknown => Callee::Unresolved(format!("?.{m}")),
        }
    }
}

#[derive(Default)]
struct Facts {
    uses: BTreeMap<&'static str, BuiltinUse>,
    unknown: Vec<String>,
    unresolved: Vec<String>,
    plots: PlotCounts,
    input_calls: Vec<NodeId>,
}

impl Facts {
    fn bump(&mut self, e: SEntry, what: u8) {
        let u = self.uses.entry(e.full()).or_insert(BuiltinUse {
            full: e.full(),
            calls: 0,
            values: 0,
            types: 0,
        });
        match what {
            0 => u.calls += 1,
            1 => u.values += 1,
            _ => u.types += 1,
        }
    }
}

struct Walker<'a, 'b> {
    cx: &'a Ctx<'b>,
    f: Facts,
    /// Plot calls that are top-level (a root statement or a root `VarDecl`'s initialiser).
    top: Vec<bool>,
}

impl Walker<'_, '_> {
    /// A chain in value (`0`) or type (`2`) position.
    fn chain_at(&mut self, id: NodeId, what: u8) {
        let Some((root, _)) = self.cx.chain(id) else {
            return;
        };
        if self.cx.bind.resolved(root).is_some() {
            return; // user code, a library member or a user type
        }
        let p = self.cx.chain_text(id);
        match self.cx.spec_hit(&p) {
            Some(e) => self.f.bump(e, what),
            // D-H1: a type namespace with no entry of its own (`array<float>`).
            None if what == 2 && spec_view::names_type(&p) => {}
            None => self.f.unknown.push(p),
        }
    }

    fn typeref(&mut self, id: NodeId) {
        if let Node::TypeRef { path, args, .. } = self.cx.ast.node(id) {
            self.chain_at(path, 2);
            for &a in args {
                self.typeref(a);
            }
        }
    }

    fn visit(&mut self, id: NodeId) {
        let ast = self.cx.ast;
        match ast.node(id) {
            Node::Ident { .. } => self.chain_at(id, 1),
            Node::Field { base, .. } => {
                if self.cx.chain(id).is_some() {
                    self.chain_at(id, 1);
                } else {
                    self.visit(base);
                }
            }
            Node::TypeRef { .. } => self.typeref(id),
            Node::Name { .. } => {}
            Node::Call {
                callee,
                type_args,
                args,
            } => {
                match self.cx.classify(callee) {
                    Callee::Spec { entry, .. } => {
                        self.f.bump(entry, 0);
                        match entry.plot() {
                            Some(pine_ir::PlotKind::Plot) => {
                                if self.top[id.0 as usize] {
                                    self.f.plots.plot_top += 1;
                                } else {
                                    self.f.plots.plot_nested += 1;
                                }
                            }
                            Some(pine_ir::PlotKind::Hline) => self.f.plots.hline += 1,
                            _ => {}
                        }
                        if entry.is_input() {
                            self.f.input_calls.push(id);
                        }
                    }
                    Callee::Unknown(p) => self.f.unknown.push(p),
                    Callee::Unresolved(m) => self.f.unresolved.push(m),
                    _ => {}
                }
                // The callee's inner nodes: a non-chain base still holds expressions.
                if self.cx.chain(callee).is_none() {
                    match ast.node(callee) {
                        Node::Field { base, .. } => self.visit(base),
                        _ => self.visit(callee),
                    }
                }
                for &t in type_args {
                    self.typeref(t);
                }
                for &a in args {
                    self.visit(a);
                }
            }
            _ => {
                let kids: Vec<NodeId> = ast.children(id).collect();
                for c in kids {
                    self.visit(c);
                }
            }
        }
    }
}

/// Parse `src` with a fresh caller-owned `SymTable` (`pine_lang::parse(src, FileId::ROOT, &mut
/// syms)`) and derive every fact of §4.8 and both hashes of §4.9. Deterministic; reads no clock;
/// never panics on any input `pine_lang::parse` accepts or refuses (I22, `scan_never_panics`).
#[must_use]
pub fn scan(src: &str) -> Scan {
    let (syms, parsed) = parse_src(src);
    let ast = match parsed {
        Ok(a) => a,
        Err(codes) => {
            return Scan {
                syms,
                dialect: None,
                kind: None,
                parse_codes: codes,
                imports: Vec::new(),
                uses: Vec::new(),
                unknown: Vec::new(),
                methods_unresolved: Vec::new(),
                inputs: Vec::new(),
                plots: PlotCounts::default(),
                h_exact: None,
                h_fork: None,
            }
        }
    };
    let bind = crate::bind::bind(&ast, &syms);
    let cx = Ctx {
        ast: &ast,
        syms: &syms,
        bind: &bind,
        dialect: ast.dialect(),
    };
    let root = ast.root();
    let body: &[NodeId] = match ast.node(root) {
        Node::Script { body } => body,
        _ => &[],
    };
    // Top-level plot positions (§4.8.5).
    let mut top = vec![false; ast.len()];
    let mut var_init: Vec<Option<NodeId>> = vec![None; ast.len()];
    for v in ast.walk(root) {
        if let crate::ast_view::Visit::Enter(id) = v {
            if let Node::VarDecl { name, init, .. } = ast.node(id) {
                var_init[init.0 as usize] = Some(name);
            }
        }
    }
    for &s in body {
        top[s.0 as usize] = true;
        if let Node::VarDecl { init, .. } = ast.node(s) {
            top[init.0 as usize] = true;
        }
    }
    // The declared kind and the imports, from the root body.
    let mut kinds = Vec::new();
    let mut imports = Vec::new();
    for &s in body {
        match ast.node(s) {
            Node::Call { callee, .. } => {
                if let Some((r, 1)) = cx.chain(callee) {
                    if bind.resolved(r).is_none() {
                        match cx.name_text(r) {
                            "indicator" => kinds.push(ScriptKind::Indicator),
                            "strategy" => kinds.push(ScriptKind::Strategy),
                            "library" => kinds.push(ScriptKind::Library),
                            _ => {}
                        }
                    }
                }
            }
            Node::Import {
                user, lib, version, ..
            } => imports.push(LibKey {
                user: cx.name_text(user).to_string(),
                lib: cx.name_text(lib).to_string(),
                ver: u32::from(version),
            }),
            _ => {}
        }
    }
    let kind = if kinds.len() == 1 {
        Some(kinds[0])
    } else {
        None
    };
    let mut w = Walker {
        cx: &cx,
        f: Facts::default(),
        top,
    };
    for &s in body {
        w.visit(s);
    }
    let mut f = w.f;
    f.unknown.sort();
    f.unknown.dedup();
    f.unresolved.sort();
    f.unresolved.dedup();
    let inputs = f
        .input_calls
        .iter()
        .map(|&c| crate::inputs::decode(&cx, c, var_init[c.0 as usize]))
        .collect();
    let mut buf = Vec::new();
    crate::canon::write(&cx, crate::canon::Profile::Exact, &mut buf);
    let h_exact = crate::sha256(&buf);
    buf.clear();
    crate::canon::write(&cx, crate::canon::Profile::Fork, &mut buf);
    let h_fork = crate::sha256(&buf);
    Scan {
        dialect: Some(ast.dialect()),
        kind,
        parse_codes: Vec::new(),
        imports,
        uses: f.uses.into_values().collect(),
        unknown: f.unknown,
        methods_unresolved: f.unresolved,
        inputs,
        plots: f.plots,
        h_exact: Some(h_exact),
        h_fork: Some(h_fork),
        syms,
    }
}

/// `version_src` (§4.7 step 5): the integer of the first line matching
/// `^\s*//\s*@version\s*=\s*(\d+)`, scanning the whole source; `None` when absent or > 255.
#[must_use]
pub fn version_src(src: &str) -> Option<u8> {
    for line in src.lines() {
        let t = line.trim_start();
        let Some(t) = t.strip_prefix("//") else {
            continue;
        };
        let t = t.trim_start();
        let Some(t) = t.strip_prefix("@version") else {
            continue;
        };
        let t = t.trim_start();
        let Some(t) = t.strip_prefix('=') else {
            continue;
        };
        let t = t.trim_start();
        let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            continue;
        }
        return digits.parse::<u8>().ok();
    }
    None
}
