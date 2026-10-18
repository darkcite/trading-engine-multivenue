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

/// I18 — the ratchet the operator grows: 8 MiB, raised to 16 MiB by the operator on 2026-10-17 when
/// the group-E strategy captures (position series over the chart window, List-of-Trades rows) landed.
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
        total <= 16 << 20,
        "tests/conformance holds {total} bytes > 16 MiB"
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

// ---- T19 completion (2026-10-17): I5a, I9, §4.3, I20, I25, §4.9.6–§4.9.7, the manifest freeze.

use pine_conformance::fixture::{Dialects, Driver};
use pine_conformance::run::{Runner, RunnerOptions, SkipReason};
use pine_ir::Dialect;

/// Pine tokens of `src` outside comments and string literals: `(token, followed_by_paren)`.
/// A token is an identifier with its `.`-joined namespace (`syminfo.tickerid`, `ta.sma`).
fn pine_tokens(src: &str) -> Vec<(String, bool)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let id = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
        } else if id(c) {
            let s = i;
            while i < b.len()
                && (id(b[i]) || (b[i] == b'.' && b.get(i + 1).is_some_and(|n| id(*n))))
            {
                i += 1;
            }
            let mut j = i;
            while j < b.len() && b[j] == b' ' {
                j += 1;
            }
            if !c.is_ascii_digit() {
                out.push((src[s..i].to_string(), b.get(j) == Some(&b'(')));
            }
        } else {
            i += 1;
        }
    }
    out
}

/// The fixture's script text, when it lives on disk (Z001–Z014 are impl-01's registry probes).
fn script_text(f: &Fixture) -> Option<String> {
    std::fs::read_to_string(workspace_root().join(&f.script)).ok()
}

/// `pin:06-r34-bar-reads` (r34, R-P26) — the probe sheet r34's F120 F121 F123 F125 read
/// `bar_index` / `close` exactly as the sheet writes them (a constant made a series without
/// arithmetic; a series zero no fold sees, `gen/group_f3.rs`). A `Syn` run steps `SynBars`
/// (`bar_index` the step, `close` one constant), so those two names are allowed there and nowhere
/// else.
const R34_BAR_READS: [&str; 4] = ["F120", "F121", "F123", "F125"];

/// I5 (first half): a `Syn` fixture's body outside its SYN-1 prelude reads no bar field.
#[test]
fn syn_fixtures_read_no_bar_data() {
    const BAR: [&str; 13] = [
        "open",
        "high",
        "low",
        "close",
        "volume",
        "hl2",
        "hlc3",
        "ohlc4",
        "hlcc4",
        "time",
        "time_close",
        "bar_index",
        "timenow",
    ];
    let mut n = 0;
    for (_, f) in disk_fixtures() {
        // pin:06-hyg-i5-parse-probes — Z001–Z014 are impl-01's registry compile probes (their
        // `script` lies outside `tests/conformance/`, carries no SYN-1 prelude and is parsed, never
        // stepped): I5 governs the scripts this crate generates.
        if f.driver != Driver::Syn
            || matches!(f.expect, Expect::Parse { .. })
            || !f.script.starts_with(pine_conformance::FIXTURE_ROOT)
        {
            continue;
        }
        let Some(src) = script_text(&f) else { continue };
        let body = src.replacen(pine_conformance::syn::SYN1_PRELUDE, "", 1);
        let r34 = R34_BAR_READS.contains(&f.id.as_str());
        for (t, _) in pine_tokens(&body) {
            if r34 && (t == "bar_index" || t == "close") {
                continue;
            }
            assert!(
                !BAR.contains(&t.as_str())
                    && !t.starts_with("syminfo.")
                    && !t.starts_with("request."),
                "{}: Syn body names `{t}`",
                f.id.as_str()
            );
        }
        n += 1;
    }
    assert!(n > 100, "{n} Syn scripts scanned");
}

/// I9: a `dialects 56` fixture's v5 and v6 outcomes are bit-identical (plots by `to_bits`, trades
/// and the run record by `==`, errors by their rendering — `run::dialect_divergence`).
///
/// `pin:06-hyg-i9-standin` — invariance is ours against ours, so a `tv` fixture is run with its
/// oracle relabelled `authored` (the stand-in syminfo is identical in both runs); Chart/SynSig
/// fixtures without a bars root skip identically in both dialects and are counted.
///
/// A run that trips an engine `debug_assert!` is a FINDING for its owner, recorded in
/// [`I9_ENGINE_FINDINGS`]; the panicking set must equal that table exactly (a fix shows as a row
/// leaving it, a new panic as red). `pin:06-hyg-i9-panic-table` — detection only: every fixture is
/// still run in both dialects and nothing is treated as a pass.
///
/// Fixed 2026-10-17 (r21), rows retired: B088/B091 (pine-std `bi_extreme` wrote the `int` result
/// of `ta.highestbars`/`ta.lowestbars` via `set_f64_arg`; now `set_i64_arg`) and F093 (pine-sema
/// lowering: a v5 W1 `const int / const int` that infer types `int` without folding it —
/// `timestamp(…)` of literals — widened both operands to f64 and fed them to `I_DIV`; it now stays
/// in the int lane, truncating, so `na(x) ? na : x` plots 0 in v5 as in v6).
///
/// Fixed r37 (2026-10-19), row retired: F120 (the probe sheet r34's P1) — pine-sema typed a
/// ternary with a series condition and literal arms (`bar_index >= 0 ? 9223372036854775807 : 0`)
/// `const` in v5, so W1's v5 `const_trunc` truncated a series division (`mul` plotted 0 in v5,
/// −2.168404344971009e−19 in v6); the ternary's qualifier now joins its condition's
/// (`pin:02-ternary-qual-cond`).
const I9_ENGINE_FINDINGS: &[(&str, &str, &str)] = &[];

#[test]
fn dialect_56_fixtures_are_invariant() {
    let new_runner = || Runner::new(RunnerOptions::new(workspace_root())).expect("runner");
    let mut runner = new_runner();
    let (mut ran, mut skipped, mut divergent) = (0u32, 0u32, Vec::new());
    let mut panicked: Vec<String> = Vec::new();
    for (_, f) in disk_fixtures() {
        if f.dialects != Dialects::BOTH || f.oracle == Oracle::TvOnly {
            continue;
        }
        let mut g = f.clone();
        if g.oracle.is_captured_kind() {
            g.oracle = Oracle::Authored;
        }
        let both = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (runner.run(&g, Dialect::V6), runner.run(&g, Dialect::V5))
        }));
        let Ok((r6, r5)) = both else {
            panicked.push(f.id.as_str().to_string());
            runner = new_runner();
            continue;
        };
        match (&r6, &r5) {
            (Err(pine_conformance::run::RunError::Skip(_)), _) => skipped += 1,
            _ => ran += 1,
        }
        if let Some(at) = pine_conformance::run::dialect_divergence(&r6, &r5) {
            let cell = |r: &Result<pine_conformance::run::RunOutcome, _>| {
                r.as_ref().ok().and_then(|o| {
                    let row = o.keys.iter().position(|k| *k == at.0)?;
                    o.plots.get(row * o.plot_ix.len() + at.1).copied()
                })
            };
            println!(
                "I9 DIVERGENT {} {at:?} v6={:?}/{:?} v5={:?}/{:?}",
                f.id.as_str(),
                cell(&r6),
                r6.as_ref().err(),
                cell(&r5),
                r5.as_ref().err()
            );
            divergent.push(f.id.as_str().to_string());
        }
    }
    println!("I9: ran {ran}, skipped {skipped}, divergent {divergent:?}, panicked {panicked:?}");
    assert!(ran > 100, "{ran} dialect-56 fixtures ran");
    let known = |k: &str| -> Vec<String> {
        I9_ENGINE_FINDINGS
            .iter()
            .filter(|(_, kind, _)| *kind == k)
            .map(|(id, _, _)| id.to_string())
            .collect()
    };
    assert_eq!(
        divergent,
        known("diverge"),
        "dialect-divergence findings table moved"
    );
    // The panic findings are pine-vm `debug_assert!` trips, compiled out of a release build
    // (pin:06-regressions-panic-debug-only).
    let want_panics = if cfg!(debug_assertions) {
        known("panic")
    } else {
        Vec::new()
    };
    assert_eq!(panicked, want_panics, "engine-panic findings table moved");
}

/// §4.3: re-derives the twinning rules from `pine_spec` — rule 1's switch rows (a `Switch` row
/// whose v5 and v6 defaults differ never has a `56` fixture), rule 2 (a `56` fixture calls no
/// builtin `pine_spec` lists in one dialect only), rule 4 (group A's bool rows are the v5/v6 twins
/// A065–A080 ↔ A081–A096) — and checks every `twin` is mutual and in the other dialect.
///
/// `pin:06-hyg-split-entry-level` — rule 2 is checked per entry (the entry's `versions` and the
/// union of its overloads' `versions`): a call's overload is not resolved here.
#[test]
fn dialect_split_matches_spec_tables() {
    let all = disk_fixtures();
    let by_id: std::collections::BTreeMap<String, &Fixture> = all
        .iter()
        .map(|(_, f)| (f.id.as_str().to_string(), f))
        .collect();
    let both = |m: pine_spec::VersionMask| m.has(Dialect::V5) && m.has(Dialect::V6);
    // Rule 1: switch rows whose dialect defaults differ.
    let split: Vec<&str> = pine_spec::SWITCHES
        .iter()
        .filter(|s| {
            s.mech == pine_spec::SwMech::Switch && s.v5 != s.v6 && !s.pin.as_str().is_empty()
        })
        .map(|s| s.pin.as_str())
        .collect();
    assert!(!split.is_empty());
    // pin:06-hyg-split-switch-pair — §4.9.2 lists `56` fixtures for a W row whose observable is
    // invariant (F001–F003 for W1), so rule 1 is checked as: a split row that has fixtures has a
    // `5` fixture and a `6` fixture, every one of them twinned.
    for p in &split {
        let carriers: Vec<&Fixture> = all
            .iter()
            .map(|(_, f)| f)
            .filter(|f| f.pins.iter().any(|q| q == p))
            .collect();
        if carriers.is_empty() {
            continue;
        }
        for d in [Dialects::V5, Dialects::V6] {
            let one: Vec<&&Fixture> = carriers.iter().filter(|f| f.dialects == d).collect();
            assert!(!one.is_empty(), "{p}: no `{}` fixture", d.text());
            for f in one {
                assert!(
                    f.twin.is_some(),
                    "{}: split row {p} untwinned",
                    f.id.as_str()
                );
            }
        }
    }
    for (_, f) in &all {
        let id = f.id.as_str();
        // Rule 2.
        if f.dialects == Dialects::BOTH {
            if let Some(src) = script_text(f) {
                for (t, call) in pine_tokens(&src) {
                    let Some(e) = pine_spec::lookup(&t).filter(|_| call) else {
                        continue;
                    };
                    let ovs = e.overloads();
                    let mut m = 0u8;
                    for o in ovs {
                        m |= o.versions.0;
                    }
                    let in_both = both(e.versions) && (ovs.is_empty() || m == 0b11);
                    assert!(
                        in_both,
                        "{id}: `56` fixture calls `{t}`, listed in one dialect only"
                    );
                }
            }
        }
        // Twins are mutual and cross-dialect.
        if let Some(t) = f.twin {
            let o = by_id
                .get(t.as_str())
                .unwrap_or_else(|| panic!("{id}: twin {} missing", t.as_str()));
            assert_eq!(
                o.twin,
                Some(f.id),
                "{id}: twin {} is not mutual",
                t.as_str()
            );
            assert!(
                f.dialects != Dialects::BOTH
                    && o.dialects != Dialects::BOTH
                    && o.dialects != f.dialects,
                "{id}: twin {} is not the other dialect",
                t.as_str()
            );
        }
    }
    // Rule 4: A(64 + k) is v5, A(80 + k) is v6, twinned with each other.
    for k in 1..=16u16 {
        let v5 = by_id[&format!("A{:03}", 64 + k)];
        let v6 = by_id[&format!("A{:03}", 80 + k)];
        assert_eq!(v5.dialects, Dialects::V5, "{}", v5.id.as_str());
        assert_eq!(v6.dialects, Dialects::V6, "{}", v6.id.as_str());
        assert_eq!(v5.twin, Some(v6.id), "{}", v5.id.as_str());
    }
}

/// I20 (§4.5.9): every skip is explained by the environment — no bars root ⇒ exactly the
/// Chart/SynSig fixtures whose expectation is not `Refuse`; tiers ⇒ exactly the inactive-tier ones;
/// `tv_only` ⇒ exactly E106, E110, D060 — in the runner's precedence (tier, tv_only, bars).
#[test]
fn skips_are_declared() {
    let opts = RunnerOptions::new(workspace_root());
    let active = opts.limits.active_tiers;
    let mut runner = Runner::new(opts).expect("runner");
    let (mut got, mut want) = (Vec::new(), Vec::new());
    let mut tv_only = Vec::new();
    for (_, f) in disk_fixtures() {
        let id = f.id.as_str().to_string();
        let bit = if (1..=8).contains(&f.tier) {
            1u8 << (f.tier - 1)
        } else {
            0
        };
        let explained = if bit & active == 0 {
            Some(SkipReason::TierInactive { tier: f.tier })
        } else if f.oracle == Oracle::TvOnly {
            tv_only.push(id.clone());
            Some(SkipReason::TvOnly)
        } else if matches!(f.driver, Driver::Chart | Driver::SynSig)
            && !matches!(f.expect, Expect::Refuse { .. })
        {
            Some(SkipReason::NoBarsRoot)
        } else {
            None
        };
        if let Some(s) = explained {
            want.push((id.clone(), s));
        }
        if let Err(pine_conformance::run::RunError::Skip(s)) =
            runner.run(&f, pine_conformance::run::primary_dialect(f.dialects))
        {
            got.push((id, s));
        }
    }
    println!("I20: {} skips", got.len());
    assert_eq!(
        got, want,
        "a skip the environment does not explain (or a missing one)"
    );
    tv_only.sort();
    assert_eq!(tv_only, ["D060", "E106", "E110"]);
}

/// I25: the Declined set of §4.6.2 is exactly `triage::DECLINED_CODES`, every code inside contract
/// §3.3's allocation (sema 3000–3999 live codes, `pine-std` 5000–5099, `pine-broker` 5100–5199);
/// the retired 3025/3033 and `DeferredBuiltin` 3024 are in no set.
#[test]
fn declined_codes_match_the_contract_ranges() {
    use pine_conformance::triage::{is_declined_code, DECLINED_CODES};
    use pine_ir::DiagCode;
    assert_eq!(
        DECLINED_CODES,
        [
            (3016, 3023),
            (3026, 3032),
            (3042, 3053),
            (5001, 5001),
            (5002, 5002),
            (5100, 5109)
        ]
    );
    for (lo, hi) in DECLINED_CODES {
        assert!(lo <= hi);
        for c in lo..=hi {
            let in_alloc = match c {
                3000..=3999 => (1..=53).contains(&(c - 3000)) && c != 3025 && c != 3033,
                5000..=5199 => true,
                _ => false,
            };
            assert!(in_alloc, "{c} outside contract §3.3");
            assert!(is_declined_code(DiagCode(c)));
        }
    }
    for c in [
        3024u16, 3025, 3033, 3015, 3041, 5000, 5003, 5099, 5110, 7001,
    ] {
        assert!(!is_declined_code(DiagCode(c)), "{c} is not Declined");
    }
}

/// §4.9.6's `pin:05-*` names, transcribed from the spec table.
const SECTION_4_9_6: [&str; 21] = [
    "pin:05-path-tie",
    "pin:05-trail-same-segment",
    "pin:05-cash-per-order",
    "pin:05-pct-equity-time",
    "pin:05-pyramiding-check",
    "pin:05-margin-funds-check",
    "pin:05-qty-rounding",
    "pin:05-bad-qty",
    "pin:05-cof-close-tick",
    "pin:05-cof-next-tick",
    "pin:05-drawdown-sampling",
    "pin:05-exit-reservation",
    "pin:05-exit-qty-per-trade",
    "pin:05-price-rounding",
    "pin:05-openprofit-fees",
    "pin:05-same-id-entry",
    "pin:05-exit-persist-flat",
    "pin:05-close-all-keeps-pending",
    "pin:05-trade-trim",
    "pin:05-v5-order-limit",
    "pin:05-fill-limits",
];

/// r29 (2026-10-17): the foreign pins the generator publishes ahead of impl-06's §4.9.6 / §4.9.7
/// linkage pass (the capture session's history-tick and tier-3 pins; the lead amends the tables,
/// then these move into the two transcriptions above and below).
const R29_AHEAD_OF_LINKAGE: [&str; 9] = [
    "pin:05-hist-tick-runs",
    "pin:05-hist-tick-next-point",
    "pin:05-hist-tick-immediately",
    "pin:05-hist-tick-cof",
    "pin:05-hist-tick-fill-first",
    "pin:04-percentile-linear-c-half",
    "pin:04-binary-search",
    "pin:04-udt-sort-ties",
    "pin:04-udt-sort-na-object",
];

/// r34 (2026-10-18, R-P26): the foreign pins the probe sheet r34's fixtures decide, published ahead
/// of the §4.9.6 / §4.9.7 linkage pass as r29's were.
const R34_AHEAD_OF_LINKAGE: [&str; 6] = [
    "pin:02-fold-sig15-ties",
    "pin:02-ta-in-loop-restart-scope",
    "pin:04-array-percentrank",
    "pin:04-cog-tv-order",
    "pin:04-pivot-levels-provisional",
    "pin:05-net-f64-fees-provisional",
];

/// §4.9.7's names, transcribed from the spec table (`pin:01-array-suffix-type` appears in two rows).
const SECTION_4_9_7: [&str; 38] = [
    "pin:00-rma-seed",
    "pin:00-dataset-start",
    "pin:00-color-consts",
    "pin:00-tf-multiplier",
    "pin:00-for-bound",
    "pin:01-mixed-indent",
    "pin:01-bracket-wrap",
    "pin:01-v5-paren-wrap",
    "pin:01-string-line-wrap",
    "pin:01-version-form",
    "pin:01-version-conflict",
    "pin:01-local-type-decl",
    "pin:01-var-field",
    "pin:01-array-suffix-type",
    "pin:01-ternary-assoc",
    "pin:01-unknown-escape",
    "pin:01-multiline-string-value",
    "pin:01-import-version-form",
    "pin:02-shadow-builtin",
    "pin:02-color-alpha",
    "pin:02-na-condition",
    "pin:02-v5-num-to-bool",
    "pin:02-var-qualifier",
    "pin:02-for-step",
    "pin:02-fn-global-assign",
    "pin:02-int-cast",
    "pin:02-once-flag",
    "pin:03-hist-dynamic-offset",
    "pin:03-float-mod",
    "pin:04-ta-reference",
    "pin:04-string-edges",
    "pin:04-color-edges",
    "pin:04-collection-edges",
    "pin:04-strategy-edges",
    "pin:04-exchange-tz",
    "pin:04-time-edges",
    "pin:04-math-random",
    "pin:10-shell-decl-args",
];

/// §7.7: every `FOREIGN_SWITCH_PINS` entry is a §4.9.6–§4.9.7 name, each exactly once, with the
/// owner its `pin:NN-` prefix names; and the two tables are covered in full (§4.9.8: 21 + 38).
#[test]
fn foreign_switch_pins_are_section_4_9_names() {
    use pine_conformance::pins::FOREIGN_SWITCH_PINS;
    let mut names: Vec<&str> = SECTION_4_9_6
        .iter()
        .chain(SECTION_4_9_7.iter())
        .chain(R29_AHEAD_OF_LINKAGE.iter())
        .chain(R34_AHEAD_OF_LINKAGE.iter())
        .copied()
        .collect();
    names.sort_unstable();
    let n = names.len();
    names.dedup();
    assert_eq!(names.len(), n, "the transcription repeats a name");
    let mut got: Vec<&str> = FOREIGN_SWITCH_PINS.iter().map(|(p, _)| *p).collect();
    got.sort_unstable();
    assert_eq!(got, names);
    for (p, owner) in FOREIGN_SWITCH_PINS {
        let nn = &p["pin:".len().."pin:".len() + 2];
        assert_ne!(nn, "06", "{p}: this spec's own pins are not foreign");
        assert_eq!(*owner, format!("impl-{nn}"), "{p}");
    }
}

/// The manifest freeze (T19): the generator-owned columns of `manifest.tsv` (every column but
/// `captured`, which the capture month fills) hash to the frozen value, and the header carries the
/// frozen counts. A deliberate catalogue change re-freezes both here, reviewed.
///
/// `pin:06-manifest-freeze-mask` — the spec freezes "the manifest" without naming its form; the
/// freeze is over the header and the generator-owned columns, so capture never trips it.
#[test]
fn manifest_is_frozen() {
    const HEADER: &str =
        "# fix 2 · pine-conf gen · A 96 B 127 C 46 D 62 E 138 F 128 Z 19 · total 616";
    // Re-frozen 2026-10-17 (R-P2): C029 gained `pin:06-na-object-field`. R-P3: C044 / D053 / D054
    // are pending (rewritten as tick-independent relations, awaiting their re-capture). Re-frozen
    // r29 (2026-10-17): E122–E132 and F099–F119 appended for the next capture session (pending;
    // F109 / F111 / F113 authored `refuse 3005`); no existing row changed. Re-frozen r37
    // (2026-10-18, R-P26): the probe sheet r34's B125–B127, E133–E138 and F120–F128 appended
    // (pending); no existing row changed.
    const FROZEN_SHA256: &str = "152758a4b3336f1a9bcc28d111efabcb0555b12afdab85cb4a1150b7c02f175c";
    let text =
        std::fs::read_to_string(workspace_root().join(pine_conformance::manifest::MANIFEST_TSV))
            .unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some(HEADER));
    let mut masked = String::new();
    let mut rows = 0;
    for l in lines {
        let cols: Vec<&str> = l.split('\t').collect();
        masked.push_str(&cols[..cols.len() - 1].join("\t"));
        masked.push('\n');
        rows += 1;
    }
    assert_eq!(rows, 616);
    let sha = core_crypto::sha256(masked.as_bytes());
    let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, FROZEN_SHA256, "manifest generator-owned columns moved");
}
