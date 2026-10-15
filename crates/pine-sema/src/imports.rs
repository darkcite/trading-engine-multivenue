// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 1: the import graph (impl-02 §4.2.2). A depth-first walk from the root assigns `FileId`s
//! in DFS pre-order and parses each library once per compilation into the compilation's own
//! `SymTable` (impl-01 §3.1) — never a cached `Ast` (§3.3).

use pine_ir::{Diag, Dialect, FileId, Span, Sym, SymTable};
use pine_lang::{Ast, Node};

use crate::code::{diag, ReasonCode};
use crate::libcache::{LibCache, LibKey};
use crate::limits::MAX_IMPORT_DEPTH;

/// One resolved `import` of a file: its namespace alias and the file it names.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Import {
    /// `alias`, or the library name when no alias is written.
    pub(crate) alias: Sym,
    /// The imported file.
    pub(crate) file: FileId,
    /// The `import` statement.
    pub(crate) span: Span,
}

/// The loaded import graph. `libs[i]` is `FileId(i + 1)` — `None` when its parse failed (the id
/// stays reserved: its diagnostics name it); `edges[f]` lists file `f`'s imports in source order
/// (the root is `FileId::ROOT`, owned by the caller).
#[derive(Debug)]
pub(crate) struct Graph {
    /// Library `Ast`s, `FileId(1..)`, DFS pre-order.
    pub(crate) libs: Vec<Option<Ast>>,
    /// Per file (root first), its resolved imports.
    pub(crate) edges: Vec<Vec<Import>>,
}

/// One `import` statement as written.
struct Written {
    key: LibKey,
    alias: Sym,
    span: Span,
}

struct Walker<'c> {
    syms: &'c mut SymTable,
    libs: &'c mut LibCache,
    diags: &'c mut Vec<Diag>,
    dialect: Dialect,
    /// `keys[i]` is the key of `FileId(i + 1)`.
    keys: Vec<LibKey>,
    stack: Vec<LibKey>,
    graph: Graph,
}

/// Run pass 1 over `root` (which is `FileId::ROOT`), collecting refusals 1, 2, 4, 34–37, 41 and
/// every library's own parse diagnostics into `diags`; continue with what loaded.
pub(crate) fn load(
    root: &Ast,
    syms: &mut SymTable,
    libs: &mut LibCache,
    diags: &mut Vec<Diag>,
) -> Graph {
    let written = written_imports(root, syms, diags);
    let mut w = Walker {
        syms,
        libs,
        diags,
        dialect: root.dialect(),
        keys: Vec::new(),
        stack: Vec::new(),
        graph: Graph {
            libs: Vec::new(),
            edges: vec![Vec::new()],
        },
    };
    w.visit(FileId::ROOT, written, 0);
    w.graph
}

/// The top-level `import`s of `ast` in source order, with steps 1's alias checks: a duplicate
/// alias is `3041`, an alias equal to a built-in namespace `3004`.
fn written_imports(ast: &Ast, syms: &SymTable, diags: &mut Vec<Diag>) -> Vec<Written> {
    let mut out: Vec<Written> = Vec::new();
    let Node::Script { body } = ast.node(ast.root()) else {
        return out;
    };
    let mut i = 0;
    while i < body.len() {
        let id = body[i];
        i += 1;
        let Node::Import {
            user,
            lib,
            version,
            alias,
        } = ast.node(id)
        else {
            continue;
        };
        let span = ast.span(id);
        let lib_sym = ast.sym(lib);
        let alias = match alias {
            Some(a) => ast.sym(a),
            None => lib_sym,
        };
        let alias_text = syms.resolve(alias);
        if out.iter().any(|w| w.alias == alias) {
            let msg = format!("duplicate import alias `{alias_text}`");
            diags.push(diag(ReasonCode::DuplicateDecl, span, msg));
            continue;
        }
        if pine_spec::is_namespace(alias_text) {
            let msg = format!("import alias `{alias_text}` is a built-in namespace");
            diags.push(diag(ReasonCode::ReservedName, span, msg));
            continue;
        }
        // COPY: a `LibKey` owns its segments (impl-02 §3.3); one per import, load time.
        let key = LibKey {
            user: syms.resolve(ast.sym(user)).to_owned(),
            lib: syms.resolve(lib_sym).to_owned(),
            ver: version,
        };
        out.push(Written { key, alias, span });
    }
    out
}

fn key_text(k: &LibKey) -> String {
    format!("{}/{}/{}", k.user, k.lib, k.ver)
}

impl Walker<'_> {
    /// Steps 2–5 of §4.2.2 for the imports of `file`, at `depth` (root = 0).
    fn visit(&mut self, file: FileId, written: Vec<Written>, depth: u32) {
        for w in written {
            if self.stack.contains(&w.key) {
                let mut path = String::new();
                let start = self.stack.iter().position(|k| *k == w.key).unwrap_or(0);
                for k in &self.stack[start..] {
                    path.push_str(&key_text(k));
                    path.push_str(" → ");
                }
                path.push_str(&key_text(&w.key));
                let mut d = diag(
                    ReasonCode::ImportCycle,
                    w.span,
                    format!("import cycle: {path}"),
                );
                d.notes.push((w.span, path));
                self.diags.push(d);
                continue;
            }
            if depth + 1 > MAX_IMPORT_DEPTH {
                let msg = format!(
                    "import {} is at depth {} (at most {MAX_IMPORT_DEPTH})",
                    key_text(&w.key),
                    depth + 1
                );
                self.diags.push(diag(ReasonCode::ImportDepth, w.span, msg));
                continue;
            }
            if let Some(k) = self.keys.iter().position(|k| *k == w.key) {
                let target = FileId(k as u16 + 1);
                self.edge(file, w.alias, target, w.span);
                continue;
            }
            let Ok(next) = u16::try_from(self.keys.len() + 1) else {
                let msg = "more than 65 535 library files in one compilation".to_owned();
                self.diags.push(diag(ReasonCode::Internal, w.span, msg));
                continue;
            };
            let src = match self.libs.source(&w.key, w.span) {
                Ok(i) => i,
                Err(d) => {
                    self.diags.push(d);
                    continue;
                }
            };
            let target = FileId(next);
            // COPY: the key is recorded once per library file of this compilation (load time).
            self.keys.push(w.key.clone());
            self.graph.edges.push(Vec::new());
            self.edge(file, w.alias, target, w.span);
            let ast = match pine_lang::parse(self.libs.loaded()[src].text(), target, self.syms) {
                Ok(ast) => ast,
                Err(ds) => {
                    self.graph.libs.push(None);
                    let too_old = ds
                        .iter()
                        .any(|d| d.code == pine_lang::code::DIALECT_TOO_OLD);
                    self.diags.extend(ds);
                    if too_old {
                        let msg =
                            format!("library {} declares a //@version below 5", key_text(&w.key));
                        self.diags
                            .push(diag(ReasonCode::DialectTooOld, w.span, msg));
                    }
                    continue;
                }
            };
            if ast.dialect() != self.dialect {
                let msg = format!(
                    "library {} is Pine v{}, the script is v{} (one dialect per program)",
                    key_text(&w.key),
                    ast.dialect() as u8,
                    self.dialect as u8
                );
                self.diags
                    .push(diag(ReasonCode::DialectMismatch, w.span, msg));
            }
            let inner = written_imports(&ast, self.syms, self.diags);
            self.graph.libs.push(Some(ast));
            self.stack.push(w.key);
            self.visit(target, inner, depth + 1);
            self.stack.pop();
        }
    }

    fn edge(&mut self, from: FileId, alias: Sym, file: FileId, span: Span) {
        self.graph.edges[usize::from(from.0)].push(Import { alias, file, span });
    }
}
