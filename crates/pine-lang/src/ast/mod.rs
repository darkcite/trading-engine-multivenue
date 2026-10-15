// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The AST arena and its read API.

use pine_ir::{Dialect, FileId, NodeId, Qual, Span, Sym};

pub(crate) mod build;
mod node;
mod validate;
mod walk;

pub(crate) use node::{nf, DocRun, RawNode, NONE};
pub use node::{AnnotKind, Annotation, AssignOp, BinOp, DeclMode, Node, NodeKind, StrForm, UnOp};
pub use validate::{AstDefect, Defect};
pub use walk::{Children, Visit, Walk};

/// A parsed source file: an arena of 32-byte POD nodes in postorder.
#[derive(Debug, Clone)]
pub struct Ast {
    pub(crate) file: FileId,
    pub(crate) dialect: Dialect,
    pub(crate) version_span: Span,
    pub(crate) sym_bound: u32,
    pub(crate) nodes: Vec<RawNode>,
    pub(crate) extra: Vec<NodeId>,
    pub(crate) annos: Vec<Annotation>,
    pub(crate) docs: Vec<DocRun>,
}

/// Fixed prefix, one list, fixed suffix: every node's children in source order.
struct Shape<'a> {
    pre: [u32; 6],
    npre: usize,
    list: &'a [NodeId],
    post: [u32; 2],
    npost: usize,
}

fn qual_of(q: u32) -> Option<Qual> {
    match q {
        1 => Some(Qual::Const),
        2 => Some(Qual::Input),
        3 => Some(Qual::Simple),
        4 => Some(Qual::Series),
        _ => None,
    }
}

fn opt(v: u32) -> Option<NodeId> {
    if v == NONE {
        None
    } else {
        Some(NodeId(v))
    }
}

impl Ast {
    /// The file every span names.
    #[must_use]
    pub fn file(&self) -> FileId {
        self.file
    }
    /// The dialect `//@version=` selected.
    #[must_use]
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }
    /// The `//@version=` annotation.
    #[must_use]
    pub fn version_span(&self) -> Span {
        self.version_span
    }
    /// Every `Sym` in this `Ast` is below this value (`SymTable::len()` after the parse).
    #[must_use]
    pub fn sym_bound(&self) -> u32 {
        self.sym_bound
    }
    /// The `Script` node: always the last node.
    #[must_use]
    pub fn root(&self) -> NodeId {
        NodeId(self.nodes.len() as u32 - 1)
    }
    /// Node count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    /// Never true: the root always exists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    #[inline]
    fn raw(&self, id: NodeId) -> &RawNode {
        &self.nodes[id.0 as usize]
    }
    /// The node's kind.
    #[must_use]
    pub fn kind(&self, id: NodeId) -> NodeKind {
        self.raw(id).kind
    }
    /// The node's span.
    #[must_use]
    pub fn span(&self, id: NodeId) -> Span {
        self.raw(id).span
    }
    /// 1 for a leaf; 1 + the tallest child otherwise; at most `MAX_DEPTH`.
    #[must_use]
    pub fn height(&self, id: NodeId) -> u32 {
        u32::from(self.raw(id).height)
    }
    /// The `Sym` of an `Ident` or `Name` (`Sym::NONE` for any other kind).
    #[must_use]
    pub fn sym(&self, id: NodeId) -> Sym {
        let r = self.raw(id);
        match r.kind {
            NodeKind::Ident | NodeKind::Name => Sym(r.a),
            _ => {
                debug_assert!(false, "Ast::sym on {:?}", r.kind);
                Sym::NONE
            }
        }
    }
    fn slice(&self, start: u32, len: u32) -> &[NodeId] {
        let s = start as usize;
        &self.extra[s..s + len as usize]
    }

    fn shape(&self, id: NodeId) -> Shape<'_> {
        let r = *self.raw(id);
        let mut s = Shape {
            pre: [NONE; 6],
            npre: 0,
            list: &[],
            post: [NONE; 2],
            npost: 0,
        };
        let pre = |v: u32, s: &mut Shape<'_>| {
            if v != NONE {
                s.pre[s.npre] = v;
                s.npre += 1;
            }
        };
        match r.kind {
            NodeKind::Script | NodeKind::Tuple | NodeKind::Block => s.list = self.slice(r.a, r.b),
            NodeKind::Error
            | NodeKind::Ident
            | NodeKind::Name
            | NodeKind::IntLit
            | NodeKind::FloatLit
            | NodeKind::BoolLit
            | NodeKind::StrLit
            | NodeKind::ColorLit
            | NodeKind::Break
            | NodeKind::Continue => {}
            NodeKind::Field
            | NodeKind::NamedArg
            | NodeKind::Hist
            | NodeKind::Binary
            | NodeKind::While
            | NodeKind::Assign
            | NodeKind::SwitchArm
            | NodeKind::Once
            | NodeKind::EnumMember => {
                pre(r.a, &mut s);
                pre(r.b, &mut s);
            }
            NodeKind::Unary => pre(r.a, &mut s),
            NodeKind::Ternary | NodeKind::If | NodeKind::Param | NodeKind::FieldDecl => {
                pre(r.a, &mut s);
                pre(r.b, &mut s);
                pre(r.c, &mut s);
            }
            NodeKind::Import => {
                pre(r.a, &mut s);
                pre(r.b, &mut s);
                pre(r.c, &mut s);
            }
            NodeKind::VarDecl => {
                pre(r.a, &mut s);
                pre(r.b, &mut s);
                pre(r.c, &mut s);
            }
            NodeKind::Call => {
                pre(r.a, &mut s);
                s.list = self.slice(r.b, r.c + r.d);
            }
            NodeKind::Switch => {
                pre(r.a, &mut s);
                s.list = self.slice(r.b, r.c);
            }
            NodeKind::For => {
                let x = r.a as usize;
                let mut k = 0;
                while k < 6 {
                    pre(self.extra[x + k].0, &mut s);
                    k += 1;
                }
            }
            NodeKind::ForIn => {
                s.list = self.slice(r.a, r.b);
                s.post = [r.c, r.d];
                s.npost = 2;
            }
            NodeKind::TupleDecl | NodeKind::TupleAssign => {
                s.list = self.slice(r.a, r.b);
                s.post[0] = r.c;
                s.npost = 1;
            }
            NodeKind::FuncDecl => {
                pre(r.a, &mut s);
                s.list = self.slice(r.b, r.c);
                s.post[0] = r.d;
                s.npost = 1;
            }
            NodeKind::TypeDecl | NodeKind::EnumDecl | NodeKind::TypeRef => {
                pre(r.a, &mut s);
                s.list = self.slice(r.b, r.c);
            }
        }
        s
    }

    /// Every compiler annotation, in source order.
    #[must_use]
    pub fn annotations(&self) -> &[Annotation] {
        &self.annos
    }

    /// The annotation run documenting statement `stmt` (empty when none).
    #[must_use]
    pub fn doc(&self, stmt: NodeId) -> &[Annotation] {
        match self.docs.binary_search_by_key(&stmt.0, |d| d.stmt) {
            Ok(i) => {
                let d = self.docs[i];
                &self.annos[d.lo as usize..(d.lo + d.len) as usize]
            }
            Err(_) => &[],
        }
    }
}

fn binop(v: u32) -> BinOp {
    match v {
        0 => BinOp::Add,
        1 => BinOp::Sub,
        2 => BinOp::Mul,
        3 => BinOp::Div,
        4 => BinOp::Rem,
        5 => BinOp::Lt,
        6 => BinOp::Le,
        7 => BinOp::Gt,
        8 => BinOp::Ge,
        9 => BinOp::Eq,
        10 => BinOp::Ne,
        11 => BinOp::And,
        _ => BinOp::Or,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(kind: NodeKind, a: u32, lo: u32, hi: u32) -> RawNode {
        RawNode {
            kind,
            flags: 0,
            height: 1,
            a,
            b: 0,
            c: 0,
            d: 0,
            span: Span::new(FileId::ROOT, lo, hi),
        }
    }

    #[test]
    fn walk_is_balanced_on_a_small_tree() {
        // x = a + 1   →  Name(x)=0, Ident(a)=1, IntLit(1)=2, Binary=3, VarDecl=4, Script=5
        let mut nodes = vec![
            leaf(NodeKind::Name, 1, 0, 1),
            leaf(NodeKind::Ident, 2, 4, 5),
        ];
        let mut one = leaf(NodeKind::IntLit, 1, 8, 9);
        one.b = 0;
        nodes.push(one);
        nodes.push(RawNode {
            kind: NodeKind::Binary,
            flags: 0,
            height: 2,
            a: 1,
            b: 2,
            c: 0,
            d: 0,
            span: Span::new(FileId::ROOT, 4, 9),
        });
        nodes.push(RawNode {
            kind: NodeKind::VarDecl,
            flags: 0,
            height: 3,
            a: NONE,
            b: 0,
            c: 3,
            d: 0,
            span: Span::new(FileId::ROOT, 0, 9),
        });
        let ast = Ast {
            file: FileId::ROOT,
            dialect: Dialect::V6,
            version_span: Span::default(),
            sym_bound: 3,
            nodes,
            extra: vec![NodeId(4)],
            annos: vec![],
            docs: vec![],
        };
        let mut depth = 0i32;
        let mut max = 0i32;
        for v in ast.walk(NodeId(4)) {
            match v {
                Visit::Enter(_) => depth += 1,
                Visit::Leave(_) => depth -= 1,
            }
            max = max.max(depth);
        }
        assert_eq!(depth, 0);
        assert_eq!(max, 3);
        assert_eq!(ast.subtree(NodeId(4)), 0..5);
        assert_eq!(
            ast.children(NodeId(3)).collect::<Vec<_>>(),
            vec![NodeId(1), NodeId(2)]
        );
        assert_eq!(ast.child_count(NodeId(4)), 2);
    }
}
