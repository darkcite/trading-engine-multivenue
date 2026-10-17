// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The generator's own tree (impl-06 §4.8.5): what `generate` builds, what `shrink` edits, and the
//! one renderer that turns it into Pine text. Not `pine-lang`'s AST. Load-time / test code only.

use pine_ir::Dialect;

use crate::gen::emit::INDENT;

/// The three scalar types the grammar generates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sc {
    /// `int`.
    Int,
    /// `float`.
    Float,
    /// `bool` — never `na` (v6, W13): no `na` literal, no history read, no `na()`-typed arm.
    Bool,
}

impl Sc {
    /// The declaration keyword.
    pub const fn keyword(self) -> &'static str {
        match self {
            Sc::Int => "int",
            Sc::Float => "float",
            Sc::Bool => "bool",
        }
    }
}

/// A SYN-1 prelude variable (§4.1.1) — the grammar's only series source; it reads no bar field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynVar {
    /// `syn` — float.
    Syn,
    /// `syni` — int.
    Syni,
    /// `synb` — bool.
    Synb,
    /// `synna` — float, `na` on ~10 % of bars.
    Synna,
}

impl SynVar {
    /// Its name in the prelude.
    pub const fn name(self) -> &'static str {
        match self {
            SynVar::Syn => "syn",
            SynVar::Syni => "syni",
            SynVar::Synb => "synb",
            SynVar::Synna => "synna",
        }
    }
}

/// What a history read `r[k]` reads: a prelude series or a global numeric slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistRef {
    /// A prelude variable (never `synb`).
    Syn(SynVar),
    /// `v<slot>` declared at global scope.
    Slot(u16),
}

/// Binary arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/` (float demands only).
    Div,
}

/// Comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `<`.
    Lt,
    /// `>`.
    Gt,
    /// `<=`.
    Le,
    /// `>=`.
    Ge,
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
}

/// An expression. Every compound renders parenthesised, so precedence never matters.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A non-negative int literal.
    LitI(i32),
    /// A non-negative float literal in hundredths (`LitF(125)` renders `1.25`).
    LitF(i32),
    /// `true` / `false`.
    LitB(bool),
    /// A prelude variable.
    Syn(SynVar),
    /// `v<slot>`.
    Slot(u16),
    /// `i<n>`, a `for` counter (int).
    Loop(u16),
    /// `r[k]`, `k` a literal in `1..=max_hist_k`.
    Hist(HistRef, u16),
    /// Arithmetic of two operands of the demanded type.
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// Comparison of two floats.
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    /// `a and b` — `b` never contains a call (W3, §4.8.2).
    And(Box<Expr>, Box<Expr>),
    /// `a or b` — `b` never contains a call.
    Or(Box<Expr>, Box<Expr>),
    /// `not a`.
    Not(Box<Expr>),
    /// `c ? a : b`.
    Tern(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `na(e)` — `e` numeric.
    IsNa(Box<Expr>),
    /// `nz(e)` — `e` numeric.
    Nz(Box<Expr>),
    /// A builtin of the derived alphabet: its `pine_spec` name, overload id and positional args.
    Call {
        /// `Entry::full_str()`.
        name: &'static str,
        /// `OverloadId` (index into `pine_spec::OVERLOADS`).
        ov: u16,
        /// One per required parameter, in order.
        args: Vec<Expr>,
    },
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `[var ]<ty> v<slot> = e`.
    Decl {
        /// Slot number (the name is `v<slot>`).
        slot: u16,
        /// Declared type.
        ty: Sc,
        /// `var` (global scope only).
        var: bool,
        /// Initialiser.
        e: Expr,
    },
    /// `v<slot> := e`.
    Assign {
        /// A visible slot.
        slot: u16,
        /// Of the slot's type.
        e: Expr,
    },
    /// `if c` / `else`.
    If {
        /// A bool expression.
        cond: Expr,
        /// Non-empty.
        then: Vec<Stmt>,
        /// May be empty (no `else`).
        els: Vec<Stmt>,
    },
    /// `for i<n> = lo to hi` — literal bounds, `hi − lo + 1 ≤ max_loop_trips`.
    For {
        /// Counter number.
        var: u16,
        /// Literal start.
        lo: i32,
        /// Literal end (inclusive).
        hi: i32,
        /// Non-empty.
        body: Vec<Stmt>,
    },
    /// `plot(e, "p<ix>")` at global scope.
    Plot {
        /// A numeric expression.
        e: Expr,
        /// Plot number (title `p<ix>`).
        ix: u16,
    },
}

/// A generated program.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// The dialect its version line names.
    pub dialect: Dialect,
    /// Global statements.
    pub stmts: Vec<Stmt>,
}

/// The `ix` of every global `plot` statement, in source order.
pub fn plot_ixs(p: &Program) -> Vec<u16> {
    p.stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Plot { ix, .. } => Some(*ix),
            _ => None,
        })
        .collect()
}

/// The plot title of plot `ix`.
pub fn plot_title(ix: u16) -> String {
    format!("p{ix}")
}

fn lit_f(o: &mut String, h: i32) {
    use std::fmt::Write as _;
    debug_assert!(h >= 0);
    let _ = write!(o, "{}.{:02}", h / 100, h % 100);
}

/// Renders `e` (load-time; allocates).
pub fn render_expr(o: &mut String, e: &Expr) {
    use std::fmt::Write as _;
    match e {
        Expr::LitI(v) => {
            let _ = write!(o, "{v}");
        }
        Expr::LitF(h) => lit_f(o, *h),
        Expr::LitB(b) => o.push_str(if *b { "true" } else { "false" }),
        Expr::Syn(s) => o.push_str(s.name()),
        Expr::Slot(s) => {
            let _ = write!(o, "v{s}");
        }
        Expr::Loop(i) => {
            let _ = write!(o, "i{i}");
        }
        Expr::Hist(r, k) => {
            match r {
                HistRef::Syn(s) => o.push_str(s.name()),
                HistRef::Slot(s) => {
                    let _ = write!(o, "v{s}");
                }
            }
            let _ = write!(o, "[{k}]");
        }
        Expr::Bin(op, a, b) => {
            let t = match op {
                BinOp::Add => " + ",
                BinOp::Sub => " - ",
                BinOp::Mul => " * ",
                BinOp::Div => " / ",
            };
            bin(o, a, t, b);
        }
        Expr::Cmp(op, a, b) => {
            let t = match op {
                CmpOp::Lt => " < ",
                CmpOp::Gt => " > ",
                CmpOp::Le => " <= ",
                CmpOp::Ge => " >= ",
                CmpOp::Eq => " == ",
                CmpOp::Ne => " != ",
            };
            bin(o, a, t, b);
        }
        Expr::And(a, b) => bin(o, a, " and ", b),
        Expr::Or(a, b) => bin(o, a, " or ", b),
        Expr::Not(a) => {
            o.push_str("(not ");
            render_expr(o, a);
            o.push(')');
        }
        Expr::Tern(c, a, b) => {
            o.push('(');
            render_expr(o, c);
            o.push_str(" ? ");
            render_expr(o, a);
            o.push_str(" : ");
            render_expr(o, b);
            o.push(')');
        }
        Expr::IsNa(a) => call(o, "na", std::slice::from_ref(a.as_ref())),
        Expr::Nz(a) => call(o, "nz", std::slice::from_ref(a.as_ref())),
        Expr::Call { name, args, .. } => call(o, name, args),
    }
}

fn bin(o: &mut String, a: &Expr, op: &str, b: &Expr) {
    o.push('(');
    render_expr(o, a);
    o.push_str(op);
    render_expr(o, b);
    o.push(')');
}

fn call(o: &mut String, name: &str, args: &[Expr]) {
    o.push_str(name);
    o.push('(');
    let mut i = 0usize;
    while i < args.len() {
        if i > 0 {
            o.push_str(", ");
        }
        render_expr(o, &args[i]);
        i += 1;
    }
    o.push(')');
}

fn render_block(o: &mut String, stmts: &[Stmt], depth: usize) {
    use std::fmt::Write as _;
    for s in stmts {
        let pad = " ".repeat(depth * INDENT);
        o.push_str(&pad);
        match s {
            Stmt::Decl { slot, ty, var, e } => {
                if *var {
                    o.push_str("var ");
                }
                let _ = write!(o, "{} v{slot} = ", ty.keyword());
                render_expr(o, e);
            }
            Stmt::Assign { slot, e } => {
                let _ = write!(o, "v{slot} := ");
                render_expr(o, e);
            }
            Stmt::If { cond, then, els } => {
                o.push_str("if ");
                render_expr(o, cond);
                o.push('\n');
                render_block(o, then, depth + 1);
                if !els.is_empty() {
                    o.push_str(&pad);
                    o.push_str("else\n");
                    render_block(o, els, depth + 1);
                }
                continue;
            }
            Stmt::For { var, lo, hi, body } => {
                let _ = writeln!(o, "for i{var} = {lo} to {hi}");
                render_block(o, body, depth + 1);
                continue;
            }
            Stmt::Plot { e, ix } => {
                o.push_str("plot(");
                render_expr(o, e);
                let _ = write!(o, ", \"{}\")", plot_title(*ix));
            }
        }
        o.push('\n');
    }
}

/// The body alone (no version line, declaration or prelude) — what `shrink` counts as lines.
pub fn render_body(p: &Program) -> String {
    let mut o = String::new();
    render_block(&mut o, &p.stmts, 0);
    o
}

/// The whole script: version line, `indicator`, the SYN-1 prelude, the body.
pub fn render(p: &Program) -> String {
    let v = match p.dialect {
        Dialect::V5 => 5,
        Dialect::V6 => 6,
    };
    let mut o = format!("//@version={v}\nindicator(\"fuzz\", overlay = false)\n");
    o.push_str(crate::syn::SYN1_PRELUDE);
    o.push_str(&render_body(p));
    o
}

/// Lines of the body (`ShrinkOutcome::lines_*`).
///
/// `pin:06-fuzz-lines-body` — §4.8.5's "< 15 lines" counts the generated body, not the fixed
/// 13-line SYN-1 prelude nor the two header lines every program shares.
pub fn body_lines(p: &Program) -> u32 {
    render_body(p).lines().count() as u32
}
