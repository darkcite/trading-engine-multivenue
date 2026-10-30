// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `Ast::validate`, `AstDefect`, `Defect` (§3.6, I5–I8).

use pine_ir::NodeId;

use super::{Ast, NodeKind};
use crate::MAX_DEPTH;

/// What `Ast::validate` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defect {
    /// A child id not below its parent (I5).
    NotPostorder,
    /// A subtree not contiguous (I5).
    NotContiguous,
    /// A child span outside its parent's, or siblings out of source order (I6).
    SpanOrder,
    /// `height` wrong or above `MAX_DEPTH` (I7).
    Height,
    /// A list range outside the extra array.
    ListRange,
    /// An `Error` node (I8).
    ErrorNode,
    /// A payload out of its domain (op, mode, qual, form, version).
    Payload,
}

/// One validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AstDefect {
    /// The offending node.
    pub node: NodeId,
    /// What is wrong.
    pub defect: Defect,
}

impl Ast {
    /// Check I5–I8; `parse` debug-asserts it, the tests run it always.
    pub fn validate(&self) -> Result<(), AstDefect> {
        let n = self.nodes.len();
        let mut i = 0usize;
        while i < n {
            let id = NodeId(i as u32);
            let r = self.nodes[i];
            if r.kind == NodeKind::Error {
                return Err(AstDefect {
                    node: id,
                    defect: Defect::ErrorNode,
                });
            }
            let cnt = self.child_count(id);
            let first = self.subtree(id).start;
            let mut expect = first;
            let mut tallest = 0u16;
            let mut prev_hi = r.span.lo;
            let mut k = 0u32;
            while k < cnt {
                let c = self.child(id, k);
                if c.0 >= id.0 {
                    return Err(AstDefect {
                        node: id,
                        defect: Defect::NotPostorder,
                    });
                }
                if self.subtree(c).start != expect {
                    return Err(AstDefect {
                        node: id,
                        defect: Defect::NotContiguous,
                    });
                }
                expect = c.0 + 1;
                let cs = self.nodes[c.0 as usize].span;
                if cs.lo < prev_hi || cs.hi > r.span.hi || cs.lo < r.span.lo {
                    return Err(AstDefect {
                        node: id,
                        defect: Defect::SpanOrder,
                    });
                }
                prev_hi = cs.hi;
                tallest = tallest.max(self.nodes[c.0 as usize].height);
                k += 1;
            }
            if cnt > 0 && expect != id.0 {
                return Err(AstDefect {
                    node: id,
                    defect: Defect::NotContiguous,
                });
            }
            if r.height != tallest + 1 || u32::from(r.height) > MAX_DEPTH {
                return Err(AstDefect {
                    node: id,
                    defect: Defect::Height,
                });
            }
            i += 1;
        }
        Ok(())
    }
}
