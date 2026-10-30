// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! CI-022 (capture 08, 2026-10-29: F019 F117 F125 F098): TradingView evaluates an additive chain
//! whose `+` has a parenthesised additive right operand as one flat chain — `a + (b - c)` is
//! `(a + b) - c`, `0.1 + (0.2 + (syn - syn))` is `((0.1 + 0.2) + syn) - syn` — inside call
//! arguments too; a variable is a boundary and `a - (b ± c)` is left as written (not captured).
//! The parser keeps no parentheses, so a right-nested additive `Binary` under `+` is exactly a
//! parenthesised one. infer and lower walk the same [`Chain`]: the terms left to right, each
//! intermediate result recorded on one of the chain's own `Binary` nodes (`hosts`, the chain's
//! root last), so lower finds every intermediate's type and fold where infer put them.

use pine_ir::NodeId;
use pine_lang::{Ast, BinOp, Node};

/// One flattened chain: `terms[0]`, then `terms[i].0` applied with `terms[i].1` for `i ≥ 1`;
/// intermediate `i` (after term `i`) is recorded on `hosts[i - 1]`; `hosts.last()` is the root.
pub(crate) struct Chain {
    pub(crate) terms: Vec<(BinOp, NodeId)>,
    pub(crate) hosts: Vec<NodeId>,
}

fn additive(ast: &Ast, n: NodeId) -> bool {
    matches!(
        ast.node(n),
        Node::Binary {
            op: BinOp::Add | BinOp::Sub,
            ..
        }
    )
}

/// The chain rooted at `root` when TradingView flattens it — `root` is `a + R` with `R` an
/// additive `Binary` — else `None` (the expression is evaluated as written).
pub(crate) fn chain(ast: &Ast, root: NodeId) -> Option<Chain> {
    match ast.node(root) {
        Node::Binary {
            op: BinOp::Add,
            rhs,
            ..
        } if additive(ast, rhs) => {}
        _ => return None,
    }
    let mut c = Chain {
        terms: Vec::new(),
        hosts: Vec::new(),
    };
    walk(ast, root, BinOp::Add, &mut c);
    // The root carries the final value; every other join keeps its walk order.
    if let Some(i) = c.hosts.iter().position(|&h| h == root) {
        let r = c.hosts.remove(i);
        c.hosts.push(r);
    }
    debug_assert_eq!(c.terms.len(), c.hosts.len() + 1);
    Some(c)
}

fn walk(ast: &Ast, n: NodeId, sign: BinOp, c: &mut Chain) {
    match ast.node(n) {
        Node::Binary {
            op: op @ (BinOp::Add | BinOp::Sub),
            lhs,
            rhs,
        } => {
            walk(ast, lhs, sign, c);
            c.hosts.push(n);
            if op == BinOp::Add && additive(ast, rhs) {
                walk(ast, rhs, BinOp::Add, c);
            } else {
                c.terms.push((op, rhs));
            }
        }
        _ => c.terms.push((sign, n)),
    }
}
