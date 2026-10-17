// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.7 — the invariants. This edition carries I26 (`license_gate_twin`, T1) and I5's
//! `gen_indents_through_one_constant` (T4).

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// impl-09 v4 §3.6 C4: the `.jsonl` corpus containers, name for name and in its order.
const C4_JSONL: [&str; 9] = [
    "index",
    "libs",
    "dedup",
    "builtins",
    "buckets",
    "topn-strategies",
    "topn-indicators",
    "libs-want",
    "fetchlog",
];

/// C1–C4 over a workspace-relative `/`-separated path (§4.10): C1 a component named `corpus`
/// (`(^|/)corpus/`), C2 `*.html.zst`, C3 `endpoints.json` / `lib.lock`, C4 the nine `.jsonl` names.
fn is_corpus_container(rel: &str) -> bool {
    let comps: Vec<&str> = rel.split('/').collect();
    let (dirs, name) = comps.split_at(comps.len() - 1);
    let name = name[0];
    dirs.contains(&"corpus")
        || name.ends_with(".html.zst")
        || name == "endpoints.json"
        || name == "lib.lock"
        || name
            .strip_suffix(".jsonl")
            .is_some_and(|stem| C4_JSONL.contains(&stem))
}

/// The `.pine` allow-list: `^(tests/conformance/|crates/[^/]+/tests/)`.
fn pine_allowed(rel: &str) -> bool {
    if rel.starts_with("tests/conformance/") {
        return true;
    }
    let mut it = rel.splitn(4, '/');
    matches!(
        (it.next(), it.next(), it.next(), it.next()),
        (Some("crates"), Some(c), Some("tests"), Some(_)) if !c.is_empty()
    )
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd {
        let p = e.expect("dir entry").path();
        let rel = p
            .strip_prefix(root)
            .expect("under root")
            .to_string_lossy()
            .replace('\\', "/");
        // Build output is never tracked.
        if rel == "target" || rel.ends_with("/target") {
            continue;
        }
        if p.is_dir() {
            walk(root, &p, out);
        } else {
            out.push(rel);
        }
    }
}

/// I26 over the filesystem, no git: returns every violation, one line each.
fn license_violations(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    walk(root, &root.join("tests/conformance"), &mut files);
    walk(root, &root.join("crates"), &mut files);
    files.sort();
    let mut bad = Vec::new();
    for rel in &files {
        if is_corpus_container(rel) {
            bad.push(format!("corpus container (impl-09 §3.6 C1-C4): {rel}"));
        }
        if rel.ends_with(".pine") {
            if !pine_allowed(rel) {
                bad.push(format!(
                    ".pine outside tests/conformance/ and crates/*/tests/: {rel}"
                ));
                continue;
            }
            let text = std::fs::read_to_string(root.join(rel)).unwrap_or_default();
            if !text
                .lines()
                .take(3)
                .any(|l| l.contains("SPDX-License-Identifier: Apache-2.0"))
            {
                bad.push(format!("missing SPDX pair in its first three lines: {rel}"));
            }
        }
    }
    bad
}

#[test]
fn license_gate_rules_match_the_makefile_regexes() {
    // §4.10's exercised cases: the allow-list and the C1-C4 container names.
    for ok in [
        "tests/conformance/fixtures/A/A001.pine",
        "tests/conformance/probes/probe-00-format.pine",
        "crates/pine-lang/tests/scripts/x.pine",
        "crates/pine-sema/tests/a/b/c.pine",
    ] {
        assert!(pine_allowed(ok), "{ok}");
    }
    for bad in [
        "crates/a/b/tests/x.pine",
        "crates/pine-lang/src/x.pine",
        "x.pine",
        "docs/x.pine",
        "tests/x.pine",
        "crates//tests/x.pine",
    ] {
        assert!(!pine_allowed(bad), "{bad}");
    }
    for c in [
        "crates/pine-lang/tests/corpus/a.pine",
        "corpus/x",
        "a/b.html.zst",
        "endpoints.json",
        "tests/conformance/lib/lib.lock",
        "x/index.jsonl",
        "topn-strategies.jsonl",
        "fetchlog.jsonl",
    ] {
        assert!(is_corpus_container(c), "{c}");
    }
    for n in [
        "crates/pine-corpus/src/lib.rs",
        "x/other.jsonl",
        "a.html",
        "x/corpus.rs",
    ] {
        assert!(!is_corpus_container(n), "{n}");
    }
}

/// I26 over the whole workspace (pine-sema's 8 test scripts carry the SPDX pair since 2026-10-17).
#[test]
fn license_gate_twin() {
    let bad = license_violations(&workspace_root());
    assert!(
        bad.is_empty(),
        "license gate twin (I26):\n  {}",
        bad.join("\n  ")
    );
}

/// I26 restricted to this crate's own trees — green, and kept green by every PVM-6 change.
#[test]
fn license_gate_twin_own_trees() {
    let root = workspace_root();
    let bad: Vec<String> = license_violations(&root)
        .into_iter()
        .filter(|l| l.contains("tests/conformance/") || l.contains("crates/pine-conformance/"))
        .collect();
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// I5: `src/gen/` indents only through `gen::emit::INDENT`, defined exactly once.
#[test]
fn gen_indents_through_one_constant() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src, &src.join("gen"), &mut files);
    assert!(!files.is_empty(), "src/gen exists");
    let mut defs = 0;
    for rel in &files {
        let text = std::fs::read_to_string(src.join(rel)).unwrap();
        for (i, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if code.contains("const INDENT") {
                defs += 1;
                assert_eq!(rel, "gen/emit.rs", "INDENT defined outside gen/emit.rs");
            }
            // A literal indentation run in a string: four spaces after a quote or a `\n`.
            assert!(
                !line.contains("\"    ") && !line.contains("\\n    ") && !line.contains('\t'),
                "{rel}:{}: indentation not through INDENT: {line}",
                i + 1
            );
        }
    }
    assert_eq!(defs, 1, "exactly one INDENT definition");
}

/// `chart.json` on disk hashes (with `core_crypto::sha256`) to `pine_spec::chart::SHA256`, the
/// `chart_sha256` every fixture pins: a stale build cannot mask an edit (§4.1.4).
#[test]
fn chart_file_hash_equals_pine_spec() {
    let on_disk = pine_conformance::chart::chart_json_sha256(&workspace_root()).unwrap();
    assert_eq!(on_disk, pine_conformance::chart::spec::SHA256);
}

/// I7 over the T4 pilot (A001–A012): `gen --check` reproduces every `.pine` and every
/// generator-owned `.fix` field byte for byte, and the generator is a pure function.
#[test]
fn gen_check_is_byte_stable() {
    let root = workspace_root();
    let diffs = pine_conformance::gen::check(&root);
    assert!(diffs.is_empty(), "gen --check:\n{}", diffs.join("\n"));
    let a = pine_conformance::gen::generate();
    assert!(a.len() >= 12, "at least the 12-fixture pilot");
    assert_eq!(a, pine_conformance::gen::generate());
    for g in &a {
        let disk = std::fs::read_to_string(root.join(&g.fix_path)).unwrap();
        let f = pine_conformance::fixture::Fixture::parse(&disk, &g.fix_path).unwrap();
        assert_eq!(f.render(), disk, "I8 on {}", g.fix_path);
        assert_eq!(
            f.script_sha256,
            core_crypto_sha(&root.join(&g.pine_path)),
            "I2"
        );
    }
}

fn core_crypto_sha(p: &Path) -> [u8; 32] {
    pine_conformance::chart::sha256_file(p).unwrap()
}
