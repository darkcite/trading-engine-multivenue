// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `NodeKind`, `Node<'a>`, the operator enums, `RawNode` (32 B) and its decoding (§3.4, §3.5, §4.9).

use pine_ir::{NodeId, Qual, Span, Sym};

use super::{binop, opt, qual_of, Ast};

/// Every node kind. Discriminants are stable and append-only (impl-09 hashes them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum NodeKind {
    /// The root.
    Script = 0,
    /// A recovery placeholder; never in an `Ast` returned by `parse`.
    Error = 1,
    /// An identifier in expression or type position (a use).
    Ident = 2,
    /// An identifier that is not a use: a declared name or a selector.
    Name = 3,
    /// Integer literal.
    IntLit = 4,
    /// Float literal.
    FloatLit = 5,
    /// `true` / `false`.
    BoolLit = 6,
    /// String literal.
    StrLit = 7,
    /// Colour literal.
    ColorLit = 8,
    /// `base.member`.
    Field = 9,
    /// A call.
    Call = 10,
    /// `name = value` in an argument list.
    NamedArg = 11,
    /// `base[offset]`.
    Hist = 12,
    /// Prefix operator.
    Unary = 13,
    /// Infix operator.
    Binary = 14,
    /// `c ? a : b`.
    Ternary = 15,
    /// `[a, b, …]` in expression position.
    Tuple = 16,
    /// `if`.
    If = 17,
    /// `switch`.
    Switch = 18,
    /// One `switch` arm.
    SwitchArm = 19,
    /// `for … to … by`.
    For = 20,
    /// `for … in`.
    ForIn = 21,
    /// `while`.
    While = 22,
    /// `once` (v6).
    Once = 23,
    /// A statement list.
    Block = 24,
    /// `break`.
    Break = 25,
    /// `continue`.
    Continue = 26,
    /// A single-variable declaration.
    VarDecl = 27,
    /// `[a, b] = …`.
    TupleDecl = 28,
    /// `[a, b] := …`.
    TupleAssign = 29,
    /// `:=` and the compound assignments.
    Assign = 30,
    /// A function or method declaration.
    FuncDecl = 31,
    /// One parameter.
    Param = 32,
    /// `type`.
    TypeDecl = 33,
    /// One UDT field.
    FieldDecl = 34,
    /// `enum`.
    EnumDecl = 35,
    /// One enum member.
    EnumMember = 36,
    /// `import`.
    Import = 37,
    /// A type in annotation position.
    TypeRef = 38,
}

impl NodeKind {
    /// Every kind, by discriminant.
    pub const ALL: [NodeKind; 39] = [
        NodeKind::Script,
        NodeKind::Error,
        NodeKind::Ident,
        NodeKind::Name,
        NodeKind::IntLit,
        NodeKind::FloatLit,
        NodeKind::BoolLit,
        NodeKind::StrLit,
        NodeKind::ColorLit,
        NodeKind::Field,
        NodeKind::Call,
        NodeKind::NamedArg,
        NodeKind::Hist,
        NodeKind::Unary,
        NodeKind::Binary,
        NodeKind::Ternary,
        NodeKind::Tuple,
        NodeKind::If,
        NodeKind::Switch,
        NodeKind::SwitchArm,
        NodeKind::For,
        NodeKind::ForIn,
        NodeKind::While,
        NodeKind::Once,
        NodeKind::Block,
        NodeKind::Break,
        NodeKind::Continue,
        NodeKind::VarDecl,
        NodeKind::TupleDecl,
        NodeKind::TupleAssign,
        NodeKind::Assign,
        NodeKind::FuncDecl,
        NodeKind::Param,
        NodeKind::TypeDecl,
        NodeKind::FieldDecl,
        NodeKind::EnumDecl,
        NodeKind::EnumMember,
        NodeKind::Import,
        NodeKind::TypeRef,
    ];

    /// Decode a raw byte.
    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        if (b as usize) < Self::ALL.len() {
            Some(Self::ALL[b as usize])
        } else {
            None
        }
    }

    /// May stand where a value is read.
    #[must_use]
    pub const fn is_expr(self) -> bool {
        matches!(
            self,
            NodeKind::Ident
                | NodeKind::IntLit
                | NodeKind::FloatLit
                | NodeKind::BoolLit
                | NodeKind::StrLit
                | NodeKind::ColorLit
                | NodeKind::Field
                | NodeKind::Call
                | NodeKind::Hist
                | NodeKind::Unary
                | NodeKind::Binary
                | NodeKind::Ternary
                | NodeKind::Tuple
                | NodeKind::If
                | NodeKind::Switch
                | NodeKind::For
                | NodeKind::ForIn
                | NodeKind::While
        )
    }

    /// A structure with a local block.
    #[must_use]
    pub const fn is_structure(self) -> bool {
        matches!(
            self,
            NodeKind::If
                | NodeKind::Switch
                | NodeKind::For
                | NodeKind::ForIn
                | NodeKind::While
                | NodeKind::Once
        )
    }
}

/// Infix operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BinOp {
    /// `+`
    Add = 0,
    /// `-`
    Sub = 1,
    /// `*`
    Mul = 2,
    /// `/`
    Div = 3,
    /// `%`
    Rem = 4,
    /// `<`
    Lt = 5,
    /// `<=`
    Le = 6,
    /// `>`
    Gt = 7,
    /// `>=`
    Ge = 8,
    /// `==`
    Eq = 9,
    /// `!=`
    Ne = 10,
    /// `and`
    And = 11,
    /// `or`
    Or = 12,
}

impl BinOp {
    /// The operators page's precedence number (higher binds tighter).
    #[must_use]
    pub const fn precedence(self) -> u8 {
        match self {
            BinOp::Mul | BinOp::Div | BinOp::Rem => 7,
            BinOp::Add | BinOp::Sub => 6,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 5,
            BinOp::Eq | BinOp::Ne => 4,
            BinOp::And => 3,
            BinOp::Or => 2,
        }
    }

    /// Source spelling.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::And => "and",
            BinOp::Or => "or",
        }
    }
}

/// Prefix operators (precedence 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum UnOp {
    /// Unary `+`.
    Pos = 0,
    /// Unary `-`.
    Neg = 1,
    /// `not`.
    Not = 2,
}

impl UnOp {
    /// Source spelling.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            UnOp::Pos => "+",
            UnOp::Neg => "-",
            UnOp::Not => "not",
        }
    }
}

/// `:=` and the compound assignments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AssignOp {
    /// `:=`
    Reassign = 0,
    /// `+=`
    Add = 1,
    /// `-=`
    Sub = 2,
    /// `*=`
    Mul = 3,
    /// `/=`
    Div = 4,
    /// `%=`
    Rem = 5,
}

impl AssignOp {
    /// Source spelling.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            AssignOp::Reassign => ":=",
            AssignOp::Add => "+=",
            AssignOp::Sub => "-=",
            AssignOp::Mul => "*=",
            AssignOp::Div => "/=",
            AssignOp::Rem => "%=",
        }
    }
}

/// A declaration mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DeclMode {
    /// No keyword.
    Plain = 0,
    /// `var`.
    Var = 1,
    /// `varip`.
    Varip = 2,
}

/// How a string literal was written (lexical only; `structurally_eq` ignores it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StrForm {
    /// `"…"`
    Double = 0,
    /// `'…'`
    Single = 1,
    /// `"""…"""` (v6)
    TripleDouble = 2,
    /// `'''…'''` (v6)
    TripleSingle = 3,
}

/// A documented compiler annotation key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AnnotKind {
    /// `//@version=`
    Version = 0,
    /// `//@description`
    Description = 1,
    /// `//@function`
    Function = 2,
    /// `//@param`
    Param = 3,
    /// `//@returns`
    Returns = 4,
    /// `//@type`
    Type = 5,
    /// `//@enum`
    Enum = 6,
    /// `//@field`
    Field = 7,
    /// `//@variable`
    Variable = 8,
    /// `//@strategy_alert_message`
    StrategyAlertMessage = 9,
}

/// One compiler annotation comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Annotation {
    /// Which key.
    pub kind: AnnotKind,
    /// Always zero.
    pub _pad: [u8; 3],
    /// The text after the key (one separator stripped, trailing whitespace trimmed).
    pub text: Sym,
    /// The whole comment, `//` through the last byte before the line terminator.
    pub span: Span,
}

const _: () = assert!(core::mem::size_of::<Annotation>() == 20);
const _: () = assert!(core::mem::align_of::<Annotation>() == 4);
const _: () = assert!(core::mem::size_of::<NodeKind>() == 1);
const _: () = assert!(core::mem::size_of::<BinOp>() == 1);
const _: () = assert!(core::mem::size_of::<AnnotKind>() == 1);

/// The typed view of one node. Built on demand; allocates nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Node<'a> {
    /// The root: every top-level statement in source order.
    Script {
        /// Statements.
        body: &'a [NodeId],
    },
    /// A recovery placeholder.
    Error,
    /// A use of an identifier.
    Ident {
        /// Its text.
        sym: Sym,
    },
    /// A declared name or a selector.
    Name {
        /// Its text.
        sym: Sym,
    },
    /// Decimal integer literal (the sign is a `Unary`).
    IntLit {
        /// Value.
        value: u64,
    },
    /// Float literal.
    FloatLit {
        /// Value (finite, non-negative).
        value: f64,
    },
    /// `true` / `false`.
    BoolLit {
        /// Value.
        value: bool,
    },
    /// String literal.
    StrLit {
        /// Escape-processed value.
        value: Sym,
        /// Lexical form.
        form: StrForm,
    },
    /// `#RRGGBB` / `#RRGGBBAA`.
    ColorLit {
        /// `0x00RRGGBB`.
        rgb: u32,
        /// `AA` as written; `None` for the six-digit form.
        alpha: Option<u8>,
    },
    /// `base.member`.
    Field {
        /// Any expression.
        base: NodeId,
        /// A `Name`.
        member: NodeId,
    },
    /// `callee<type_args>(args)`.
    Call {
        /// Any expression.
        callee: NodeId,
        /// `TypeRef`s; empty without `<…>`.
        type_args: &'a [NodeId],
        /// Expressions and `NamedArg`s, in source order.
        args: &'a [NodeId],
    },
    /// `name = value` inside an argument list.
    NamedArg {
        /// A `Name`.
        name: NodeId,
        /// Expression.
        value: NodeId,
    },
    /// `base[offset]`.
    Hist {
        /// Expression.
        base: NodeId,
        /// Expression.
        offset: NodeId,
    },
    /// Prefix operator.
    Unary {
        /// Operator.
        op: UnOp,
        /// Expression.
        operand: NodeId,
    },
    /// Infix operator.
    Binary {
        /// Operator.
        op: BinOp,
        /// Left operand.
        lhs: NodeId,
        /// Right operand.
        rhs: NodeId,
    },
    /// `cond ? then : otherwise`.
    Ternary {
        /// Condition.
        cond: NodeId,
        /// Selected when true.
        then: NodeId,
        /// Selected when false.
        otherwise: NodeId,
    },
    /// `[a, b, …]`.
    Tuple {
        /// Expressions.
        items: &'a [NodeId],
    },
    /// `if` / `else if` / `else`.
    If {
        /// Condition.
        cond: NodeId,
        /// A `Block`.
        then: NodeId,
        /// A `Block`, or an `If` for `else if`.
        otherwise: Option<NodeId>,
    },
    /// `switch`.
    Switch {
        /// `None` for the subject-less form.
        subject: Option<NodeId>,
        /// `SwitchArm`s in source order.
        arms: &'a [NodeId],
    },
    /// `pattern => body` or `=> body`.
    SwitchArm {
        /// `None` for the default arm.
        pattern: Option<NodeId>,
        /// A `Block`.
        body: NodeId,
    },
    /// `for counter = from to to [by step]`.
    For {
        /// A `TypeRef` before the counter (accepted, pin-free leniency).
        ty: Option<NodeId>,
        /// A `Name`.
        counter: NodeId,
        /// Expression.
        from: NodeId,
        /// Expression.
        to: NodeId,
        /// Expression.
        step: Option<NodeId>,
        /// A `Block`.
        body: NodeId,
    },
    /// `for x in c` / `for [a, b] in c`.
    ForIn {
        /// One `Name`, or two for the bracketed form.
        binders: &'a [NodeId],
        /// Expression.
        collection: NodeId,
        /// A `Block`.
        body: NodeId,
    },
    /// `while cond`.
    While {
        /// Condition.
        cond: NodeId,
        /// A `Block`.
        body: NodeId,
    },
    /// `once [cond]` (v6).
    Once {
        /// Condition; `None` means `true`.
        cond: Option<NodeId>,
        /// A `Block`.
        body: NodeId,
    },
    /// Statements; the last one is the block's value.
    Block {
        /// Statements.
        stmts: &'a [NodeId],
    },
    /// `break`.
    Break,
    /// `continue`.
    Continue,
    /// `[export] [var|varip] [qual] [type] name = init`.
    VarDecl {
        /// `export` (v6 constant export).
        export: bool,
        /// Declaration mode.
        mode: DeclMode,
        /// `const` / `simple` / `series`.
        qual: Option<Qual>,
        /// A `TypeRef`.
        ty: Option<NodeId>,
        /// A `Name`.
        name: NodeId,
        /// Expression or structure.
        init: NodeId,
    },
    /// `[a, b] = init`.
    TupleDecl {
        /// `Name`s (`_` included).
        binders: &'a [NodeId],
        /// Expression or structure.
        init: NodeId,
    },
    /// `[a, b] := value`.
    TupleAssign {
        /// `Ident`s.
        targets: &'a [NodeId],
        /// Expression or structure.
        value: NodeId,
    },
    /// `target op value`.
    Assign {
        /// Operator.
        op: AssignOp,
        /// An `Ident` or a `Field`.
        target: NodeId,
        /// Expression or structure.
        value: NodeId,
    },
    /// `[export] [method] name(params) => body`.
    FuncDecl {
        /// `export`.
        export: bool,
        /// `method`.
        method: bool,
        /// A `Name`.
        name: NodeId,
        /// `Param`s.
        params: &'a [NodeId],
        /// A `Block`.
        body: NodeId,
    },
    /// `[qual] [type] name [= default]`.
    Param {
        /// `const` / `simple` / `series`.
        qual: Option<Qual>,
        /// A `TypeRef`.
        ty: Option<NodeId>,
        /// A `Name`.
        name: NodeId,
        /// Expression.
        default: Option<NodeId>,
    },
    /// `[export] type Name` + fields.
    TypeDecl {
        /// `export`.
        export: bool,
        /// A `Name`.
        name: NodeId,
        /// `FieldDecl`s.
        fields: &'a [NodeId],
    },
    /// `[varip] type name [= default]`.
    FieldDecl {
        /// `varip`.
        varip: bool,
        /// A `TypeRef`.
        ty: NodeId,
        /// A `Name`.
        name: NodeId,
        /// Expression.
        default: Option<NodeId>,
    },
    /// `[export] enum Name` + members.
    EnumDecl {
        /// `export`.
        export: bool,
        /// A `Name`.
        name: NodeId,
        /// `EnumMember`s.
        members: &'a [NodeId],
    },
    /// `name [= title]`.
    EnumMember {
        /// A `Name`.
        name: NodeId,
        /// Expression (a const string).
        title: Option<NodeId>,
    },
    /// `import user/lib/version [as alias]`.
    Import {
        /// A `Name`.
        user: NodeId,
        /// A `Name`.
        lib: NodeId,
        /// `1..=65535`.
        version: u16,
        /// A `Name`.
        alias: Option<NodeId>,
    },
    /// `path`, `path<args>` or `path[]`.
    TypeRef {
        /// An `Ident` or a `Field` chain of them.
        path: NodeId,
        /// `TypeRef`s.
        args: &'a [NodeId],
        /// Written `path[]`.
        array_suffix: bool,
    },
}

pub(crate) const NONE: u32 = u32::MAX;

pub(crate) mod nf {
    pub(crate) const EXPORT: u8 = 1 << 0;
    pub(crate) const METHOD: u8 = 1 << 1;
    pub(crate) const VARIP: u8 = 1 << 2;
    pub(crate) const ARRAY_SUFFIX: u8 = 1 << 3;
    pub(crate) const HAS_ALPHA: u8 = 1 << 4;
    pub(crate) const FORM_SHIFT: u8 = 5;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct RawNode {
    pub(crate) kind: NodeKind,
    pub(crate) flags: u8,
    pub(crate) height: u16,
    pub(crate) a: u32,
    pub(crate) b: u32,
    pub(crate) c: u32,
    pub(crate) d: u32,
    pub(crate) span: Span,
}

const _: () = assert!(core::mem::size_of::<RawNode>() == 32);
const _: () = assert!(core::mem::align_of::<RawNode>() == 4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct DocRun {
    pub(crate) stmt: u32,
    pub(crate) lo: u32,
    pub(crate) len: u32,
}

const _: () = assert!(core::mem::size_of::<DocRun>() == 12);

impl Ast {
    /// The typed view.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Node<'_> {
        let r = *self.raw(id);
        match r.kind {
            NodeKind::Script => Node::Script {
                body: self.slice(r.a, r.b),
            },
            NodeKind::Error => Node::Error,
            NodeKind::Ident => Node::Ident { sym: Sym(r.a) },
            NodeKind::Name => Node::Name { sym: Sym(r.a) },
            NodeKind::IntLit => Node::IntLit {
                value: u64::from(r.a) | (u64::from(r.b) << 32),
            },
            NodeKind::FloatLit => Node::FloatLit {
                value: f64::from_bits(u64::from(r.a) | (u64::from(r.b) << 32)),
            },
            NodeKind::BoolLit => Node::BoolLit { value: r.a != 0 },
            NodeKind::StrLit => Node::StrLit {
                value: Sym(r.a),
                form: match (r.flags >> nf::FORM_SHIFT) & 3 {
                    0 => StrForm::Double,
                    1 => StrForm::Single,
                    2 => StrForm::TripleDouble,
                    _ => StrForm::TripleSingle,
                },
            },
            NodeKind::ColorLit => Node::ColorLit {
                rgb: r.a,
                alpha: if r.flags & nf::HAS_ALPHA != 0 {
                    Some(r.b as u8)
                } else {
                    None
                },
            },
            NodeKind::Field => Node::Field {
                base: NodeId(r.a),
                member: NodeId(r.b),
            },
            NodeKind::Call => Node::Call {
                callee: NodeId(r.a),
                type_args: self.slice(r.b, r.c),
                args: self.slice(r.b + r.c, r.d),
            },
            NodeKind::NamedArg => Node::NamedArg {
                name: NodeId(r.a),
                value: NodeId(r.b),
            },
            NodeKind::Hist => Node::Hist {
                base: NodeId(r.a),
                offset: NodeId(r.b),
            },
            NodeKind::Unary => Node::Unary {
                op: match r.b {
                    0 => UnOp::Pos,
                    1 => UnOp::Neg,
                    _ => UnOp::Not,
                },
                operand: NodeId(r.a),
            },
            NodeKind::Binary => Node::Binary {
                op: binop(r.c),
                lhs: NodeId(r.a),
                rhs: NodeId(r.b),
            },
            NodeKind::Ternary => Node::Ternary {
                cond: NodeId(r.a),
                then: NodeId(r.b),
                otherwise: NodeId(r.c),
            },
            NodeKind::Tuple => Node::Tuple {
                items: self.slice(r.a, r.b),
            },
            NodeKind::If => Node::If {
                cond: NodeId(r.a),
                then: NodeId(r.b),
                otherwise: opt(r.c),
            },
            NodeKind::Switch => Node::Switch {
                subject: opt(r.a),
                arms: self.slice(r.b, r.c),
            },
            NodeKind::SwitchArm => Node::SwitchArm {
                pattern: opt(r.a),
                body: NodeId(r.b),
            },
            NodeKind::For => {
                let x = r.a as usize;
                let e = &self.extra[x..x + 6];
                Node::For {
                    ty: opt(e[0].0),
                    counter: e[1],
                    from: e[2],
                    to: e[3],
                    step: opt(e[4].0),
                    body: e[5],
                }
            }
            NodeKind::ForIn => Node::ForIn {
                binders: self.slice(r.a, r.b),
                collection: NodeId(r.c),
                body: NodeId(r.d),
            },
            NodeKind::While => Node::While {
                cond: NodeId(r.a),
                body: NodeId(r.b),
            },
            NodeKind::Once => Node::Once {
                cond: opt(r.a),
                body: NodeId(r.b),
            },
            NodeKind::Block => Node::Block {
                stmts: self.slice(r.a, r.b),
            },
            NodeKind::Break => Node::Break,
            NodeKind::Continue => Node::Continue,
            NodeKind::VarDecl => Node::VarDecl {
                export: r.flags & nf::EXPORT != 0,
                mode: match r.d & 0xFF {
                    0 => DeclMode::Plain,
                    1 => DeclMode::Var,
                    _ => DeclMode::Varip,
                },
                qual: qual_of((r.d >> 8) & 0xFF),
                ty: opt(r.a),
                name: NodeId(r.b),
                init: NodeId(r.c),
            },
            NodeKind::TupleDecl => Node::TupleDecl {
                binders: self.slice(r.a, r.b),
                init: NodeId(r.c),
            },
            NodeKind::TupleAssign => Node::TupleAssign {
                targets: self.slice(r.a, r.b),
                value: NodeId(r.c),
            },
            NodeKind::Assign => Node::Assign {
                op: match r.c {
                    0 => AssignOp::Reassign,
                    1 => AssignOp::Add,
                    2 => AssignOp::Sub,
                    3 => AssignOp::Mul,
                    4 => AssignOp::Div,
                    _ => AssignOp::Rem,
                },
                target: NodeId(r.a),
                value: NodeId(r.b),
            },
            NodeKind::FuncDecl => Node::FuncDecl {
                export: r.flags & nf::EXPORT != 0,
                method: r.flags & nf::METHOD != 0,
                name: NodeId(r.a),
                params: self.slice(r.b, r.c),
                body: NodeId(r.d),
            },
            NodeKind::Param => Node::Param {
                qual: qual_of(r.d),
                ty: opt(r.a),
                name: NodeId(r.b),
                default: opt(r.c),
            },
            NodeKind::TypeDecl => Node::TypeDecl {
                export: r.flags & nf::EXPORT != 0,
                name: NodeId(r.a),
                fields: self.slice(r.b, r.c),
            },
            NodeKind::FieldDecl => Node::FieldDecl {
                varip: r.flags & nf::VARIP != 0,
                ty: NodeId(r.a),
                name: NodeId(r.b),
                default: opt(r.c),
            },
            NodeKind::EnumDecl => Node::EnumDecl {
                export: r.flags & nf::EXPORT != 0,
                name: NodeId(r.a),
                members: self.slice(r.b, r.c),
            },
            NodeKind::EnumMember => Node::EnumMember {
                name: NodeId(r.a),
                title: opt(r.b),
            },
            NodeKind::Import => Node::Import {
                user: NodeId(r.a),
                lib: NodeId(r.b),
                version: r.d as u16,
                alias: opt(r.c),
            },
            NodeKind::TypeRef => Node::TypeRef {
                path: NodeId(r.a),
                args: self.slice(r.b, r.c),
                array_suffix: r.flags & nf::ARRAY_SUFFIX != 0,
            },
        }
    }
}
