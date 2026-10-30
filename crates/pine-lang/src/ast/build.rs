// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The postorder builder (§4.9): nodes are appended after their children, each node's `height`
//! is `1 + max(child heights)`, and a node that would exceed `MAX_DEPTH` is not built.

use pine_ir::{Dialect, FileId, NodeId, Span, SymTable};

use super::{Annotation, Ast, DocRun, NodeKind, RawNode};
use crate::MAX_DEPTH;

/// A node would be taller than `MAX_DEPTH` (the caller reports `code::NESTING_TOO_DEEP`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TooDeep;

/// The arena under construction, plus the reused list stack of §4.9.
#[derive(Debug)]
pub(crate) struct Builder {
    ast: Ast,
    /// Items of the lists being parsed, innermost last; a list is `scratch[mark..]`.
    scratch: Vec<NodeId>,
}

impl Builder {
    /// An empty arena for one file, pre-sized from the source length (§4.9).
    pub(crate) fn new(file: FileId, dialect: Dialect, src_len: usize) -> Self {
        Self {
            ast: Ast {
                file,
                dialect,
                version_span: Span::default(),
                sym_bound: 0,
                nodes: Vec::with_capacity(src_len / 4 + 1),
                extra: Vec::with_capacity(src_len / 8 + 1),
                annos: Vec::new(),
                docs: Vec::new(),
                in_run: Vec::new(),
            },
            scratch: Vec::with_capacity(64),
        }
    }

    /// A childless node (height 1): never too deep.
    pub(crate) fn leaf(&mut self, kind: NodeKind, flags: u8, a: u32, b: u32, span: Span) -> NodeId {
        let id = NodeId(self.ast.nodes.len() as u32);
        let (c, d) = (0, 0);
        self.ast.nodes.push(RawNode {
            kind,
            flags,
            height: 1,
            a,
            b,
            c,
            d,
            span,
        });
        id
    }

    /// A node over already-built children (slots per §4.9); its `height` is computed here.
    /// `Err(TooDeep)` leaves the arena unchanged.
    pub(crate) fn node(
        &mut self,
        kind: NodeKind,
        flags: u8,
        slots: [u32; 4],
        span: Span,
    ) -> Result<NodeId, TooDeep> {
        let id = NodeId(self.ast.nodes.len() as u32);
        let [a, b, c, d] = slots;
        self.ast.nodes.push(RawNode {
            kind,
            flags,
            height: 0,
            a,
            b,
            c,
            d,
            span,
        });
        let mut tallest = 0u16;
        let n = self.ast.child_count(id);
        let mut k = 0u32;
        while k < n {
            let ch = self.ast.child(id, k);
            tallest = tallest.max(self.ast.nodes[ch.0 as usize].height);
            k += 1;
        }
        if u32::from(tallest) + 1 > MAX_DEPTH {
            self.ast.nodes.pop();
            return Err(TooDeep);
        }
        self.ast.nodes[id.0 as usize].height = tallest + 1;
        Ok(id)
    }

    /// The span of a built node.
    pub(crate) fn span(&self, id: NodeId) -> Span {
        self.ast.span(id)
    }

    /// The kind of a built node.
    pub(crate) fn kind(&self, id: NodeId) -> NodeKind {
        self.ast.kind(id)
    }

    /// OR `flags` into a built node's flags (`export const` marks its `VarDecl`).
    pub(crate) fn set_flags(&mut self, id: NodeId, flags: u8) {
        self.ast.nodes[id.0 as usize].flags |= flags;
    }

    /// Open a list: its items are the ones `item` pushes until `close_list(mark)`.
    pub(crate) fn mark(&self) -> usize {
        self.scratch.len()
    }

    /// One list item, in source order.
    pub(crate) fn item(&mut self, id: NodeId) {
        self.scratch.push(id);
    }

    /// Close the list opened at `mark`: its items move to `extra`; returns `(start, len)`.
    pub(crate) fn close_list(&mut self, mark: usize) -> (u32, u32) {
        let start = self.ast.extra.len() as u32;
        let len = (self.scratch.len() - mark) as u32;
        // COPY: node-id list relocation at load, 4 B × list length, once per list — the items of
        // nested lists interleave on the reused scratch stack while parsing; the alternative
        // (sibling links) would break §4.9's borrowed `&[NodeId]` lists.
        self.ast.extra.extend_from_slice(&self.scratch[mark..]);
        self.scratch.truncate(mark);
        (start, len)
    }

    /// Record statement `stmt`'s doc run `annos[lo..lo + len]` (statements arrive in id order).
    pub(crate) fn doc(&mut self, stmt: NodeId, lo: u32, len: u32) {
        self.ast.docs.push(DocRun {
            stmt: stmt.0,
            lo,
            len,
        });
    }

    /// The finished `Ast`; the last node built must be the `Script` root.
    pub(crate) fn finish(
        mut self,
        version_span: Span,
        syms: &SymTable,
        annos: Vec<Annotation>,
    ) -> Ast {
        self.ast.version_span = version_span;
        self.ast.sym_bound = syms.len() as u32;
        let mut in_run = vec![false; annos.len()];
        let mut k = 0usize;
        while k < self.ast.docs.len() {
            let d = self.ast.docs[k];
            let mut i = d.lo as usize;
            while i < (d.lo + d.len) as usize {
                in_run[i] = true;
                i += 1;
            }
            k += 1;
        }
        self.ast.annos = annos;
        self.ast.in_run = in_run;
        self.ast
    }
}

#[cfg(test)]
mod tests {
    use pine_ir::{Dialect, FileId, Span, SymTable};

    use super::{Builder, TooDeep};
    use crate::ast::{Node, NodeKind};
    use crate::MAX_DEPTH;

    fn sp(lo: u32, hi: u32) -> Span {
        Span::new(FileId::ROOT, lo, hi)
    }

    #[test]
    fn heights_are_one_plus_the_tallest_child() {
        let mut b = Builder::new(FileId::ROOT, Dialect::V6, 16);
        let x = b.leaf(NodeKind::Ident, 0, 1, 0, sp(1, 2));
        assert_eq!(b.ast.height(x), 1);
        let neg = b
            .node(NodeKind::Unary, 0, [x.0, 1, 0, 0], sp(0, 2))
            .unwrap();
        assert_eq!(b.ast.height(neg), 2);
        let m = b.mark();
        b.item(neg);
        let (start, len) = b.close_list(m);
        let root = b
            .node(NodeKind::Script, 0, [start, len, 0, 0], sp(0, 2))
            .unwrap();
        assert_eq!(b.ast.height(root), 3);
        let ast = b.finish(Span::default(), &SymTable::new(), Vec::new());
        assert_eq!(ast.root(), root);
        assert_eq!(ast.subtree(neg), 0..2);
        assert!(ast.validate().is_ok());
    }

    #[test]
    fn nested_lists_close_innermost_first() {
        let mut b = Builder::new(FileId::ROOT, Dialect::V6, 16);
        let outer = b.mark();
        let a = b.leaf(NodeKind::Ident, 0, 1, 0, sp(1, 2));
        b.item(a);
        let inner = b.mark();
        let c = b.leaf(NodeKind::Ident, 0, 2, 0, sp(4, 5));
        b.item(c);
        let (s1, l1) = b.close_list(inner);
        let t = b
            .node(NodeKind::Tuple, 0, [s1, l1, 0, 0], sp(3, 6))
            .unwrap();
        b.item(t);
        let (s0, l0) = b.close_list(outer);
        let root = b
            .node(NodeKind::Tuple, 0, [s0, l0, 0, 0], sp(0, 7))
            .unwrap();
        let ast = b.finish(Span::default(), &SymTable::new(), Vec::new());
        match ast.node(root) {
            Node::Tuple { items } => assert_eq!(items, &[a, t]),
            other => panic!("{other:?}"),
        }
        match ast.node(t) {
            Node::Tuple { items } => assert_eq!(items, &[c]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_node_above_max_depth_is_not_built() {
        let mut b = Builder::new(FileId::ROOT, Dialect::V6, 16);
        let mut top = b.leaf(NodeKind::Ident, 0, 1, 0, sp(0, 1));
        let mut h = 1u32;
        while h < MAX_DEPTH {
            top = b
                .node(NodeKind::Unary, 0, [top.0, 1, 0, 0], sp(0, 1))
                .unwrap();
            h += 1;
        }
        assert_eq!(b.ast.height(top), MAX_DEPTH);
        let before = b.ast.len();
        assert_eq!(
            b.node(NodeKind::Unary, 0, [top.0, 1, 0, 0], sp(0, 1)),
            Err(TooDeep)
        );
        assert_eq!(b.ast.len(), before);
    }
}
