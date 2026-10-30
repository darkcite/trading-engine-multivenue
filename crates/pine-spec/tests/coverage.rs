// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `coverage()` against a naive recount of the JSON (I29), the scope rules (I30), and the
//! per-tier × per-dialect table for the completion note.

#[path = "../build/json.rs"]
mod json;

use json::{Node, Val};
use pine_ir::Dialect;
use pine_spec::{coverage, Coverage, Scope, Tier, ENTRIES};

const TIERS: [Tier; 4] = [Tier::T1, Tier::T2, Tier::T3, Tier::T4];
const DIALECTS: [Dialect; 2] = [Dialect::V5, Dialect::V6];

fn field<'n, 'a>(n: &'n Node<'a>, k: &str) -> &'n Node<'a> {
    match &n.v {
        Val::Obj(f) => &f.iter().find(|(q, _)| q == k).expect(k).1,
        _ => panic!("object expected for {k}"),
    }
}

fn list<'n, 'a>(n: &'n Node<'a>) -> &'n [Node<'a>] {
    match &n.v {
        Val::Arr(a) => a,
        _ => panic!("array expected"),
    }
}

fn int(n: &Node<'_>) -> i64 {
    match n.v {
        Val::Int(i) => i,
        _ => panic!("integer expected"),
    }
}

/// The naive count: walk the raw JSON, no lowering.
fn recount(tier: Tier, d: Dialect) -> Coverage {
    let root = json::parse(include_bytes!("../spec/builtins.json")).expect("parses");
    let want = d as i64;
    let mut c = Coverage::default();
    for e in list(field(&root, "entries")) {
        if int(field(e, "tier")) != tier as i64 {
            continue;
        }
        let declined = matches!(&field(e, "scope").v, Val::Str(s) if s == "declined");
        for o in list(field(e, "overloads")) {
            if list(field(o, "versions")).iter().any(|v| int(v) == want) {
                c.overloads += 1;
                if declined {
                    c.declined += 1;
                } else {
                    c.in_scope += 1;
                }
            }
        }
    }
    c
}

#[test]
fn coverage_matches_independent_count() {
    for t in TIERS {
        for d in DIALECTS {
            assert_eq!(coverage(t, d), recount(t, d), "{t:?} {d:?}");
        }
    }
}

#[test]
fn scope_rules_hold() {
    for e in ENTRIES.iter() {
        let n = e.full.as_str();
        match e.scope {
            Scope::Declined => assert!(e.decline.len > 0, "{n}: declined without a reason"),
            Scope::In => assert_eq!(e.decline.len, 0, "{n}: in scope with a reason"),
        }
        let rowless = e.ov_lo == e.ov_hi && e.val_lo == e.val_hi;
        if e.tier != Tier::T4 {
            assert!(!rowless || e.is_type, "{n}: name-only outside T4");
        }
    }
}

#[test]
fn coverage_table() {
    println!("| tier | dialect | overloads | in scope | declined |");
    println!("|---|---|---|---|---|");
    for t in TIERS {
        for d in DIALECTS {
            let c = coverage(t, d);
            println!(
                "| {t:?} | {d:?} | {} | {} | {} |",
                c.overloads, c.in_scope, c.declined
            );
            assert_eq!(c, recount(t, d));
        }
    }
}
