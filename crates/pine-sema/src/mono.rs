// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 4's Instantiations (impl-02 §4.6). Pass 3 typed one **TypeInst** per `(declaration,
//! ArgKey)` (`infer::Typed::insts`; the typing stack and its `3012` live there, where typing
//! recurses). This module expands them into the **Instantiation tree** of §4.6.3 — one node per
//! call site of the fully inlined program, `InstId(0)` the root's top level, numbered depth-first
//! in execution order at the call site — and enforces the caps of §4.6.1 (`3013`:
//! `MAX_MONO_TOTAL` in the program, `MAX_MONO_PER_FN` of one declaration). `3014` (parameters) is
//! pass 2's (`resolve.rs`). The walk stops at the first cap it crosses, so an exponential call
//! graph costs at most `MAX_MONO_TOTAL + 1` nodes.

use std::collections::HashMap;

use pine_ir::{Diag, NodeId, Span, SymTable};
use pine_lang::{Ast, Node};

use crate::code::{diag, ReasonCode};
use crate::infer::{Callee, Typed};
use crate::limits::{MAX_MONO_PER_FN, MAX_MONO_TOTAL};

/// An Instantiation's index; `InstId(0)` is the root's top level (§4.6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct InstId(pub(crate) u32);

/// One Instantiation: `(parent Instantiation, call NodeId)` (§4.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Instantiation {
    /// The calling Instantiation (`None` for the root).
    pub(crate) parent: Option<InstId>,
    /// The call node, in the parent's TypeInst's file (`None` for the root).
    pub(crate) call: Option<NodeId>,
    /// The TypeInst it lowers: an index into `Typed::insts`.
    pub(crate) type_inst: u32,
}

/// The expanded program (§4.6.3).
#[derive(Debug, Default)]
pub(crate) struct Tree {
    /// `nodes[i]` is `InstId(i)`: parents before children, siblings in execution order.
    pub(crate) nodes: Vec<Instantiation>,
    /// `(parent, call)` → child: a lookup index, never iterated.
    index: HashMap<(u32, u32), u32>,
}

impl Tree {
    /// The Instantiation the user-function call `call` of `parent` expands to.
    pub(crate) fn child(&self, parent: InstId, call: NodeId) -> Option<InstId> {
        self.index.get(&(parent.0, call.0)).map(|&i| InstId(i))
    }
}

/// Expand the root's TypeInst into the Instantiation tree; a crossed cap is one `3013` and an
/// empty tree. `files[f]` is file `f`'s `Ast` (root first, then the libraries in `FileId` order).
pub(crate) fn build(
    t: &Typed,
    files: &[Option<&Ast>],
    syms: &SymTable,
    diags: &mut Vec<Diag>,
) -> Tree {
    let mut tree = Tree::default();
    let mut per_fn: HashMap<(usize, u32), usize> = HashMap::new(); // INDEX-ONLY
                                                                   // Pre-order with an explicit stack: pop, number, push the callees in reverse. Each entry
                                                                   // carries the file its call node lives in (the caller's).
    let root = Instantiation {
        parent: None,
        call: None,
        type_inst: t.root as u32,
    };
    let mut todo = vec![(root, 0usize)];
    while let Some((x, call_file)) = todo.pop() {
        let id = tree.nodes.len() as u32;
        let at = call_span(files, call_file, x.call);
        if tree.nodes.len() >= MAX_MONO_TOTAL {
            let msg = format!(
                "more than {MAX_MONO_TOTAL} function instantiations in the program (MAX_MONO_TOTAL)"
            );
            diags.push(diag(ReasonCode::MonoExplosion, at, msg));
            return Tree::default();
        }
        let inst = &t.insts[x.type_inst as usize];
        if let Some(d) = inst.decl {
            let n = per_fn.entry((inst.file, d.0)).or_insert(0);
            *n += 1;
            if *n > MAX_MONO_PER_FN {
                let (name, decl_at) = fn_name(files, syms, inst.file, d);
                let msg = format!(
                    "more than {MAX_MONO_PER_FN} instantiations of `{name}` (MAX_MONO_PER_FN)"
                );
                let mut dg = diag(ReasonCode::MonoExplosion, at, msg);
                dg.notes
                    .push((decl_at, format!("`{name}` is declared here")));
                diags.push(dg);
                return Tree::default();
            }
        }
        if let (Some(p), Some(c)) = (x.parent, x.call) {
            tree.index.insert((p.0, c.0), id);
        }
        tree.nodes.push(x);
        // One call site per call node (a node typed twice still calls once).
        let mut seen: Vec<NodeId> = Vec::new();
        let mut kids = Vec::new();
        for &(n, c) in &inst.calls {
            if let Callee::User(ti) = c {
                if !seen.contains(&n) {
                    seen.push(n);
                    kids.push(Instantiation {
                        parent: Some(InstId(id)),
                        call: Some(n),
                        type_inst: ti,
                    });
                }
            }
        }
        while let Some(k) = kids.pop() {
            todo.push((k, inst.file));
        }
    }
    tree
}

/// The span of a call node of file `file` (the root's span for the root).
fn call_span(files: &[Option<&Ast>], file: usize, call: Option<NodeId>) -> Span {
    match (files.get(file).copied().flatten(), call) {
        (Some(a), Some(c)) => a.span(c),
        (Some(a), None) => a.span(a.root()),
        (None, _) => Span::default(),
    }
}

/// A function declaration's name and the span of its name.
pub(crate) fn fn_name(
    files: &[Option<&Ast>],
    syms: &SymTable,
    file: usize,
    d: NodeId,
) -> (String, Span) {
    let Some(a) = files.get(file).copied().flatten() else {
        return (String::new(), Span::default());
    };
    match a.node(d) {
        Node::FuncDecl { name, .. } => (syms.resolve(a.sym(name)).to_owned(), a.span(name)),
        _ => (String::new(), a.span(d)),
    }
}
