// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `libs.rs` (I11–I13, I30).

mod common;

use std::collections::BTreeMap;

use common::{corpus, S};
use pine_corpus::libs::{cycle_text, walk, Graph, MAX_IMPORT_DEPTH};
use pine_corpus::scan::LibKey;

fn k(s: &str) -> LibKey {
    let mut p = s.split('/');
    LibKey {
        user: p.next().unwrap().into(),
        lib: p.next().unwrap().into(),
        ver: p.next().unwrap().parse().unwrap(),
    }
}

#[test]
fn libs_plan_emits_every_unresolved_import() {
    let g = Graph {
        mains: vec![("s".into(), vec![k("a/x/1"), k("b/y/2")])],
        cached: BTreeMap::from([(k("a/x/1"), vec![k("c/z/3")])]),
        permanent: BTreeMap::new(),
    };
    let (p, reach) = walk(&g);
    let wanted: Vec<String> = p.wants.iter().map(|w| w.key.text()).collect();
    assert_eq!(wanted, ["b/y/2", "c/z/3"]);
    assert_eq!(p.wants[1].depth, 2);
    assert_eq!(p.resolved, 1);
    assert!(reach["s"].unresolved.contains("pending"));
}

#[test]
fn libs_cycle_is_detected_and_reported() {
    let g = Graph {
        mains: vec![("s".into(), vec![k("a/x/1")])],
        cached: BTreeMap::from([
            (k("a/x/1"), vec![k("b/y/2")]),
            (k("b/y/2"), vec![k("a/x/1")]),
        ]),
        permanent: BTreeMap::new(),
    };
    let (p, reach) = walk(&g);
    assert_eq!(p.cycles.len(), 1);
    assert_eq!(cycle_text(&p.cycles[0]), "a/x/1 -> b/y/2 -> a/x/1");
    assert!(reach["s"].unresolved.contains("cycle"));
}

#[test]
fn libs_two_versions_of_one_library_are_not_a_cycle() {
    let g = Graph {
        mains: vec![("s".into(), vec![k("a/x/2")])],
        cached: BTreeMap::from([(k("a/x/2"), vec![k("a/x/1")]), (k("a/x/1"), vec![])]),
        permanent: BTreeMap::new(),
    };
    let (p, reach) = walk(&g);
    assert!(p.cycles.is_empty());
    assert!(reach["s"].unresolved.is_empty());
    assert_eq!(reach["s"].libs.len(), 2);
}

#[test]
fn libs_case_collision_is_refused() {
    let g = Graph {
        mains: vec![("s".into(), vec![k("alice/Utils/1"), k("alice/utils/1")])],
        cached: BTreeMap::new(),
        permanent: BTreeMap::new(),
    };
    let (p, reach) = walk(&g);
    assert_eq!(p.case_collisions.len(), 1);
    assert!(p.wants.is_empty());
    assert!(reach["s"].unresolved.contains("case_collision"));
}

#[test]
fn libs_unsafe_segment_is_never_wanted() {
    let bad = [
        LibKey {
            user: "..".into(),
            lib: "x".into(),
            ver: 1,
        },
        LibKey {
            user: "a\\b".into(),
            lib: "c".into(),
            ver: 1,
        },
        LibKey {
            user: "a".into(),
            lib: "b\0c".into(),
            ver: 1,
        },
    ];
    let g = Graph {
        mains: vec![("s".into(), bad.to_vec())],
        cached: BTreeMap::new(),
        permanent: BTreeMap::new(),
    };
    let (p, _) = walk(&g);
    assert!(p.wants.is_empty());
    assert_eq!(p.bad_segments.len(), 3);
    let (_t, c) = corpus("seg", &[], &[]);
    for b in &bad {
        assert!(c.lib_path(b).is_none());
    }
}

#[test]
fn libs_depth_beyond_the_bound_is_recorded_not_wanted() {
    // A chain l1 -> l2 -> … -> l9; l9 sits at depth 9.
    let mut cached = BTreeMap::new();
    for i in 1..=8 {
        cached.insert(k(&format!("u/l{i}/1")), vec![k(&format!("u/l{}/1", i + 1))]);
    }
    let g = Graph {
        mains: vec![("s".into(), vec![k("u/l1/1")])],
        cached,
        permanent: BTreeMap::new(),
    };
    let (p, reach) = walk(&g);
    assert!(p.wants.is_empty());
    assert_eq!(p.over_depth.len(), 1);
    assert_eq!(p.over_depth[0].depth, 9);
    assert!(reach["s"].unresolved.contains("over_depth"));
}

#[test]
fn libs_max_import_depth_is_eight() {
    assert_eq!(MAX_IMPORT_DEPTH, 8);
}

#[test]
fn libs_permanent_outcomes_are_not_wanted_again() {
    let g = Graph {
        mains: vec![("s".into(), vec![k("carol/Gone/2")])],
        cached: BTreeMap::new(),
        permanent: BTreeMap::from([(k("carol/Gone/2"), "lib_missing")]),
    };
    let (p, reach) = walk(&g);
    assert!(p.wants.is_empty());
    assert!(reach["s"].unresolved.contains("lib_missing"));
}

#[test]
fn libs_verify_lock_detects_a_mutated_library() {
    let lib = "//@version=5\nlibrary(\"L\")\nexport f(float x) =>\n    x\n";
    let (_t, c) = corpus(
        "lock",
        &[S::new("s", "//@version=5\nindicator(\"t\")\nplot(close)\n")],
        &[("zed/L/1", lib)],
    );
    assert_eq!(pine_corpus::libs::verify_lock(&c), Ok(1));
    std::fs::write(
        c.root().join("lib/zed/L/1.pine"),
        "//@version=5\nlibrary(\"L\")\n",
    )
    .unwrap();
    assert_eq!(
        pine_corpus::libs::verify_lock(&c),
        Err(vec!["zed/L/1".to_string()])
    );
}

#[test]
fn libs_cache_layout_is_user_lib_ver() {
    let (_t, c) = corpus("layout", &[], &[]);
    let p = c.lib_path(&k("alice/Utils/3")).unwrap();
    assert!(p.ends_with("lib/alice/Utils/3.pine"));
    // The shared lock fixture verifies against the libcache fixture.
    let text =
        std::fs::read_to_string(common::fixtures().join("libcache/lib.lock.fixture")).unwrap();
    let (entries, bad) = pine_corpus::libs::parse_lock(&text);
    assert!(bad.is_empty());
    for (key, h) in entries {
        let b = std::fs::read(common::fixtures().join(format!("libcache/{key}.pine"))).unwrap();
        assert_eq!(pine_corpus::sha256(&b), h);
    }
}

#[test]
fn libs_plan_reads_a_corpus_and_writes_the_copy_list() {
    let (_t, c) = corpus(
        "want",
        &[S::new("u", &common::fixture("import_user.pine"))],
        &[],
    );
    let p = pine_corpus::build::libs(&c).unwrap();
    assert_eq!(p.wants.len(), 1);
    assert_eq!(p.wants[0].key.text(), "bob/Helpers/1");
    let w = common::out(&c, "libs-want.jsonl");
    let row = pine_corpus::schema::WantRow::read(&w).unwrap();
    assert_eq!(
        (
            row.user.as_str(),
            row.lib.as_str(),
            row.ver,
            row.depth,
            row.dependents
        ),
        ("bob", "Helpers", 1, 1, 1)
    );
}
