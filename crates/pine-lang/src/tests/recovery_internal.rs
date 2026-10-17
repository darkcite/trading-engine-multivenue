// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! D1-d on the partial arena (§7.9): *k* planted errors leave exactly *k* `Error` nodes.
//!
//! Ruling (lead, 2026-10-15): the count covers diagnostics sited at a token. Diagnostics that
//! by design drop no node are excluded: 1006, 1009, 1011, 1012 (layout and stray brackets) and
//! every 2104 (the gate/layout/literal/`once`/`export const` dialect checks).

use pine_ir::{FileId, SymTable};

use crate::ast::NodeKind;
use crate::parse::parse_partial;

/// The planted fragments of `tests/recovery.rs`, one diagnostic each.
const PLANTS: [&str; 19] = [
    "x = 1 +\n",
    "y = 2)\n",
    "1 := 2\n",
    "z = 3 $ 4\n",
    "if c\nw = 0\n",
    "break\n",
    "switch\n    => 1\n    a => 2\n",
    "q = 1\n    r = 2\n",
    "k = #GG0000\n",
    "import a/b\n",
    "if c\n    f(x) => x\n",
    "var [a, b] = f()\n",
    "v = 1.2.3\n",
    "if a\n        x = 1\n",
    "for i = 0 10\n    x = 1\n",
    "x =\n",
    "switch\n    => 1\n    => 2\n",
    "float x\n",
    "export 5\n",
];

/// Codes that by design leave no `Error` node.
const NODELESS: [u16; 5] = [1006, 1009, 1011, 1012, 2104];

#[test]
fn planted_errors_leave_exactly_k_error_nodes() {
    let ks = [1usize, 1, 2, 2, 3, 3, 4, 5, 6, 8, 12, 19];
    let mut start = 0usize;
    for k in ks {
        let mut src = String::from("//@version=6\n");
        let mut i = 0usize;
        while i < k {
            src.push_str(PLANTS[(start + i) % PLANTS.len()]);
            i += 1;
        }
        start += 3;
        let mut syms = SymTable::new();
        let (partial, diags) = parse_partial(&src, FileId::ROOT, &mut syms);
        assert_eq!(diags.len(), k, "{src}");
        let ast = partial.ast.expect("an arena");
        let errors = (0..ast.len() as u32)
            .filter(|&n| ast.kind(pine_ir::NodeId(n)) == NodeKind::Error)
            .count();
        let sited = diags
            .iter()
            .filter(|d| !NODELESS.contains(&d.code.0))
            .count();
        assert_eq!(errors, sited, "{src}");
    }
}

#[test]
fn a_clean_source_has_no_error_node() {
    let mut syms = SymTable::new();
    let (partial, diags) = parse_partial(
        "//@version=6\nx = f(1, 2)\nif x\n    y = 1\n",
        FileId::ROOT,
        &mut syms,
    );
    assert!(diags.is_empty());
    let ast = partial.ast.expect("an arena");
    assert!(ast.validate().is_ok());
}
