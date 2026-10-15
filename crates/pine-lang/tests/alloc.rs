// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Allocation invariants (impl-01 §7.11/§7.12), measured with the workspace's
//! `core_alloc::CountingAllocator`. One test in this binary: the counters are process-wide, so
//! no other test may allocate while it measures (the runner thread is not counted, HK14).

use std::fs;
use std::path::Path;

use core_alloc::{AllocGuard, CountingAllocator};
use pine_ir::{FileId, NodeId, SymTable};
use pine_lang::{parse, structurally_eq, Node, Visit};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator::new();

/// Every fixture and probe script.
fn fixtures() -> Vec<String> {
    let mut out = Vec::new();
    for sub in ["tests/scripts", "tests/probes"] {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(sub);
        for e in fs::read_dir(&dir).expect("dir") {
            let p = e.expect("entry").path();
            if p.extension().is_some_and(|x| x == "pine") {
                out.push(fs::read_to_string(&p).expect("read"));
            }
        }
    }
    out
}

/// D1-e: `Ast::node` / `children` / `subtree` / `doc` / `height` / `walk`, `SymTable::resolve`
/// and `structurally_eq` allocate nothing.
fn read_paths_are_zero_alloc() {
    for src in fixtures() {
        let mut syms = SymTable::new();
        let Ok(a) = parse(&src, FileId::ROOT, &mut syms) else {
            continue;
        };
        let mut s2 = SymTable::new();
        let b = parse(&src, FileId::ROOT, &mut s2).expect("parses twice");
        let g = AllocGuard::new();
        let mut acc = 0u64;
        let mut i = 0u32;
        while (i as usize) < a.len() {
            let id = NodeId(i);
            if let Node::Ident { sym } | Node::Name { sym } = a.node(id) {
                acc += syms.resolve(sym).len() as u64;
            }
            for c in a.children(id) {
                acc += u64::from(c.0);
            }
            let r = a.subtree(id);
            acc += u64::from(r.end - r.start) + a.doc(id).len() as u64;
            i += 1;
        }
        for v in a.walk(a.root()) {
            if let Visit::Enter(id) = v {
                acc += u64::from(a.height(id));
            }
        }
        let eq = structurally_eq(&a, &syms, &b, &s2);
        g.assert_zero();
        assert!(eq && acc > 0);
    }
}

/// Never allocates unboundedly: a parse's cumulative allocation is linear in the source length.
fn allocation_is_bounded_by_source_length() {
    let p = "//@version=6\n";
    let inputs = [
        format!("{p}{}", "x = 1 +\n".repeat(20_000)),
        format!("{p}{}", "@".repeat(100_000)),
        format!("{p}{}a{}", "(".repeat(50_000), ")".repeat(50_000)),
        format!("{p}x = \"{}\"\n", "\\n".repeat(100_000)),
        format!("{p}{}", "//@variable v\n".repeat(20_000)),
        format!("{p}{}", "a,".repeat(50_000)),
        format!("{p}{}", "if c\n    x\n".repeat(10_000)),
        format!("{p}{}", "\n".repeat(100_000)),
        format!("{p}x = [{}]\n", "1,".repeat(50_000)),
    ];
    for src in &inputs {
        let mut syms = SymTable::new();
        let g = AllocGuard::new();
        let r = parse(src, FileId::ROOT, &mut syms);
        let (_, used, _) = g.delta();
        drop(r);
        let bound = 96 * src.len() as u64 + (4 << 20);
        assert!(
            used <= bound,
            "{used} bytes allocated for a {}-byte source",
            src.len()
        );
    }
}

#[test]
fn allocation_invariants() {
    read_paths_are_zero_alloc();
    allocation_is_bounded_by_source_length();
}
