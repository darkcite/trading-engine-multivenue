// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `Children`, `Walk`, `Visit` and `subtree` (§3.6).

use pine_ir::NodeId;

use super::Ast;
use crate::MAX_DEPTH;

/// A traversal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visit {
    /// Before the node's children.
    Enter(NodeId),
    /// After them.
    Leave(NodeId),
}

/// The children of one node, in source order.
#[derive(Debug, Clone)]
pub struct Children<'a> {
    ast: &'a Ast,
    id: NodeId,
    next: u32,
    count: u32,
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        if self.next >= self.count {
            return None;
        }
        let c = self.ast.child(self.id, self.next);
        self.next += 1;
        Some(c)
    }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let r = (self.count - self.next) as usize;
        (r, Some(r))
    }
}

impl ExactSizeIterator for Children<'_> {}

/// A depth-first walk yielding `Enter`/`Leave` events; a fixed stack of `MAX_DEPTH` frames.
#[derive(Debug, Clone)]
pub struct Walk<'a> {
    ast: &'a Ast,
    stack: [(u32, u32); MAX_DEPTH as usize],
    len: usize,
    start: Option<NodeId>,
}

impl Iterator for Walk<'_> {
    type Item = Visit;
    fn next(&mut self) -> Option<Visit> {
        if let Some(s) = self.start.take() {
            self.stack[0] = (s.0, 0);
            self.len = 1;
            return Some(Visit::Enter(s));
        }
        if self.len == 0 {
            return None;
        }
        let top = self.len - 1;
        let (id, k) = self.stack[top];
        if k < self.ast.child_count(NodeId(id)) {
            self.stack[top].1 = k + 1;
            let c = self.ast.child(NodeId(id), k);
            debug_assert!(self.len < MAX_DEPTH as usize, "height bound (I7) broken");
            self.stack[self.len] = (c.0, 0);
            self.len += 1;
            return Some(Visit::Enter(c));
        }
        self.len -= 1;
        Some(Visit::Leave(NodeId(id)))
    }
}

impl Ast {
    /// Number of children.
    #[must_use]
    pub fn child_count(&self, id: NodeId) -> u32 {
        let s = self.shape(id);
        (s.npre + s.list.len() + s.npost) as u32
    }

    /// Child `k` in source order; panics when `k >= child_count(id)`.
    #[must_use]
    pub fn child(&self, id: NodeId, k: u32) -> NodeId {
        let s = self.shape(id);
        let k = k as usize;
        if k < s.npre {
            return NodeId(s.pre[k]);
        }
        let k = k - s.npre;
        if k < s.list.len() {
            return s.list[k];
        }
        let k = k - s.list.len();
        assert!(k < s.npost, "child index out of range");
        NodeId(s.post[k])
    }

    /// The children, in source order.
    #[must_use]
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            ast: self,
            id,
            next: 0,
            count: self.child_count(id),
        }
    }

    /// A depth-first walk of `from`'s subtree.
    #[must_use]
    pub fn walk(&self, from: NodeId) -> Walk<'_> {
        Walk {
            ast: self,
            stack: [(0, 0); MAX_DEPTH as usize],
            len: 0,
            start: Some(from),
        }
    }

    /// The contiguous id range of `id`'s subtree (postorder: `id` is its last element).
    #[must_use]
    pub fn subtree(&self, id: NodeId) -> core::ops::Range<u32> {
        let mut first = id;
        while self.child_count(first) > 0 {
            first = self.child(first, 0);
        }
        first.0..id.0 + 1
    }
}
