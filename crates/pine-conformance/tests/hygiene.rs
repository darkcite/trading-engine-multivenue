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

use pine_conformance::fixture::{Expect, Fixture, Oracle, ReadingBody, Stamp};

/// Every `.fix` on disk with its text, in path order.
fn disk_fixtures() -> Vec<(String, Fixture)> {
    let root = workspace_root();
    let mut files = Vec::new();
    walk(&root, &root.join("tests/conformance/fixtures"), &mut files);
    files.sort();
    files
        .into_iter()
        .filter(|r| r.ends_with(".fix"))
        .map(|r| {
            let t = std::fs::read_to_string(root.join(&r)).unwrap();
            let f = Fixture::parse(&t, &r).unwrap_or_else(|e| panic!("{e}"));
            (t, f)
        })
        .collect()
}

/// I1.
#[test]
fn chart_sha_and_capture_stamp_in_every_fixture() {
    for (_, f) in disk_fixtures() {
        let id = f.id.as_str();
        assert_eq!(
            f.chart_sha256,
            pine_conformance::chart::spec::SHA256,
            "{id}"
        );
        if f.oracle.is_captured_kind() {
            if f.expect != Expect::Pending {
                assert!(
                    matches!(&f.captured, Stamp::At(u) if pine_conformance::fixture::is_capture_utc(u)),
                    "{id}: captured"
                );
                assert!(matches!(f.tv_build, Stamp::At(_)), "{id}: tv_build");
                assert!(f.captured_in.is_some(), "{id}: captured_in");
            }
        } else {
            assert_eq!(f.captured, Stamp::NotApplicable, "{id}");
            assert_eq!(f.tv_build, Stamp::NotApplicable, "{id}");
        }
    }
}

/// I2 and I8 over every `.fix` on disk.
#[test]
fn script_sha256_matches_disk() {
    let root = workspace_root();
    let all = disk_fixtures();
    assert!(all.len() >= 300);
    for (t, f) in &all {
        assert_eq!(f.render(), *t, "I8 on {}", f.id.as_str());
        let p = root.join(&f.script);
        if p.is_file() {
            assert_eq!(f.script_sha256, core_crypto_sha(&p), "{}", f.id.as_str());
        } else {
            // Z001–Z014: impl-01's probes outside the fixture tree (registry).
            assert!(f.script.starts_with("crates/pine-lang/"), "{}", f.script);
        }
    }
}

/// I3.
#[test]
fn every_pine_carries_the_spdx_pair() {
    let root = workspace_root();
    let mut files = Vec::new();
    walk(&root, &root.join("tests/conformance"), &mut files);
    let pines: Vec<&String> = files.iter().filter(|r| r.ends_with(".pine")).collect();
    assert!(pines.len() >= 300);
    for r in pines {
        let t = std::fs::read_to_string(root.join(r)).unwrap();
        assert!(
            t.starts_with(pine_conformance::gen::emit::SPDX_PAIR),
            "{r}: SPDX pair not on lines 1-2"
        );
    }
}

/// I4.
#[test]
fn no_fixture_carries_ohlcv() {
    for (_, f) in disk_fixtures() {
        let id = f.id.as_str();
        for p in &f.plots {
            let t = p.title.as_str();
            assert!(
                !["open", "high", "low", "close", "volume"].contains(&t),
                "{id}: {t}"
            );
            let rest = t
                .strip_prefix("f_")
                .and_then(|r| r.strip_prefix(id))
                .and_then(|r| r.strip_prefix('_'))
                .unwrap_or_else(|| panic!("{id}: {t} is not f_{id}_…"));
            assert!(
                (1..=12).contains(&rest.len())
                    && rest
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{id}: {t}"
            );
        }
    }
}

/// I6.
#[test]
fn fixture_ids_are_unique_and_dense() {
    let all = disk_fixtures();
    let mut per: Vec<(char, Vec<u16>)> = Vec::new();
    for (_, f) in &all {
        let g = f.id.group().letter();
        match per.iter_mut().find(|(x, _)| *x == g) {
            Some((_, v)) => v.push(f.id.number()),
            None => per.push((g, vec![f.id.number()])),
        }
    }
    for (g, mut v) in per {
        assert!("ABCDEFZ".contains(g), "{g}");
        v.sort_unstable();
        let want: Vec<u16> = (1..=v.len() as u16).collect();
        assert_eq!(v, want, "group {g} not dense from 001");
    }
}

/// I12 (half 1): every `live_refuse` fixture is refused with exactly its code under
/// `live_bound = true` in every declared dialect — `compile_only` directly, no bar files.
///
/// `pin:06-hyg-standin-syminfo` — the refusal is a compile-time check; the private tree's
/// `syminfo.tsv` is the stand-in's rows (a `tv` fixture refuses the stand-in by design).
#[test]
fn lookahead_fixtures_are_refused_when_live_bound() {
    use pine_conformance::run::{RunError, Runner, RunnerOptions};
    let (t, fixtures) = live_tree("live", |f| f.live_refuse.is_some());
    assert!(!fixtures.is_empty());
    let mut r = Runner::new(RunnerOptions::new(t.clone())).unwrap();
    for f in &fixtures {
        let want = f.live_refuse.unwrap();
        for d in [pine_ir::Dialect::V5, pine_ir::Dialect::V6] {
            if !f.dialects.has(d) {
                continue;
            }
            match r.compile_only(f, d, true) {
                Err(e @ RunError::Compile(_)) => {
                    assert_eq!(e.code(), Some(want), "{} v{}", f.id.as_str(), d as u8)
                }
                other => panic!(
                    "{} v{}: want refusal {}, got {:?}",
                    f.id.as_str(),
                    d as u8,
                    want.0,
                    other.err()
                ),
            }
        }
    }
    let _ = std::fs::remove_dir_all(&t);
}

/// I12 (half 2): D017 / D018 (leak-free `lookahead_on`) compile under `live_bound = true`.
#[test]
fn leak_free_lookahead_compiles_live_bound() {
    use pine_conformance::run::{Runner, RunnerOptions};
    let (t, fixtures) = live_tree("leakfree", |f| ["D017", "D018"].contains(&f.id.as_str()));
    assert_eq!(fixtures.len(), 2);
    let mut r = Runner::new(RunnerOptions::new(t.clone())).unwrap();
    for f in &fixtures {
        let d = pine_conformance::run::primary_dialect(f.dialects);
        assert!(
            r.compile_only(f, d, true).is_ok(),
            "{}: {:?}",
            f.id.as_str(),
            r.compile_only(f, d, true).err()
        );
    }
    let _ = std::fs::remove_dir_all(&t);
}

/// A private tree with the stand-in as `syminfo.tsv` and the scripts of the selected fixtures.
fn live_tree(tag: &str, pick: impl Fn(&Fixture) -> bool) -> (PathBuf, Vec<Fixture>) {
    let root = workspace_root();
    let t = std::env::temp_dir().join(format!("pine-conf-hyg-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    let picked: Vec<Fixture> = disk_fixtures()
        .into_iter()
        .map(|(_, f)| f)
        .filter(|f| pick(f))
        .collect();
    for f in &picked {
        let dst = t.join(&f.script);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(root.join(&f.script), dst).unwrap();
    }
    let standin =
        std::fs::read_to_string(root.join("tests/conformance/syminfo.standin.tsv")).unwrap();
    let rows: String = standin
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::create_dir_all(t.join("tests/conformance")).unwrap();
    std::fs::write(t.join("tests/conformance/syminfo.tsv"), rows).unwrap();
    (t, picked)
}

/// I18.
#[test]
fn fixture_payload_is_under_the_ratchet() {
    let root = workspace_root();
    let mut files = Vec::new();
    walk(&root, &root.join("tests/conformance"), &mut files);
    let total: u64 = files
        .iter()
        .map(|r| std::fs::metadata(root.join(r)).map_or(0, |m| m.len()))
        .sum();
    assert!(
        total <= 8 << 20,
        "tests/conformance holds {total} bytes > 8 MiB"
    );
}

/// Source files of the crate (`src/`), `/`-relative to the crate.
fn crate_files(sub: &str) -> Vec<(String, String)> {
    let c = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    walk(&c, &c.join(sub), &mut files);
    files.sort();
    files
        .into_iter()
        .filter(|r| r.ends_with(".rs"))
        .map(|r| {
            let t = std::fs::read_to_string(c.join(&r)).unwrap();
            (r, t)
        })
        .collect()
}

/// I21: one compile spelling, one instantiation path.
#[test]
fn compile_edge_is_isolated() {
    for (r, t) in crate_files("src") {
        for tok in [
            "pine_sema::compile(",
            "pine_vm::verify(",
            "VmInstance::new(",
        ] {
            if t.contains(tok) {
                assert_eq!(r, "src/run.rs", "{tok} outside src/run.rs");
            }
        }
    }
}

/// I22 (D18): no eight-decimal scale token in `src/`, `tests/` or `tests/conformance/`.
#[test]
fn no_1e8_anywhere() {
    let toks = [
        concat!("1", "e8"),
        concat!("100_000", "_000"),
        concat!("_1", "e8"),
    ];
    let mut all = crate_files("src");
    all.extend(crate_files("tests"));
    let root = workspace_root();
    let mut conf = Vec::new();
    walk(&root, &root.join("tests/conformance"), &mut conf);
    for r in conf {
        if let Ok(t) = std::fs::read_to_string(root.join(&r)) {
            all.push((r, t));
        }
    }
    for (r, t) in &all {
        // pin:06-hyg-i22-own-name — §7.7 names this test `no_…_anywhere` with the token inside.
        let t = &t.replace(concat!("no_1", "e8_anywhere"), "");
        for tok in toks {
            // A token: not inside a longer number or identifier.
            let mut from = 0;
            while let Some(i) = t[from..].find(tok) {
                let (a, b) = (from + i, from + i + tok.len());
                let before = t[..a].chars().next_back();
                let after = t[b..].chars().next();
                let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
                let num_after = after.is_some_and(|c| c.is_ascii_digit() || c == '_');
                assert!(
                    word(before) && !tok.starts_with('_') || num_after,
                    "{r}: `{tok}` (D18)"
                );
                from = b;
            }
        }
    }
}

/// I23 (P5): no clock in `src/`.
#[test]
fn no_clock_in_the_crate() {
    for (r, t) in crate_files("src") {
        for tok in [
            concat!("System", "Time"),
            concat!("Inst", "ant::"),
            concat!("UNIX_", "EPOCH"),
        ] {
            assert!(!t.contains(tok), "{r}: {tok}");
        }
    }
}

/// I24: every `Refuse` code (expectation or reading) lies in 1000–5199; a sema code is
/// `3000 + n`, `n ∈ 1..=53` except the retired 25 and 33.
///
/// `pin:06-hyg-i24-ranges` — contract §3.3's per-crate ranges are checked at the sema block
/// (3000–3999) only; the rest at the parser's 1000–5199 bound.
#[test]
fn refusal_codes_are_contract_codes() {
    let ok = |c: u16| {
        (1000..=5199).contains(&c)
            && (!(3000..=3999).contains(&c) || {
                let n = c - 3000;
                (1..=53).contains(&n) && n != 25 && n != 33
            })
    };
    for (_, f) in disk_fixtures() {
        let id = f.id.as_str();
        if let Expect::Refuse { code } = f.expect {
            assert!(ok(code.0), "{id}: refuse {}", code.0);
        }
        if let Some(c) = f.live_refuse {
            assert!(ok(c.0), "{id}: live_refuse {}", c.0);
        }
        for r in &f.readings {
            if let ReadingBody::Refuse(code) = r.body {
                assert!(ok(code.0), "{id}: reading refuse {}", code.0);
            }
        }
        let _ = Oracle::Tv;
    }
}
