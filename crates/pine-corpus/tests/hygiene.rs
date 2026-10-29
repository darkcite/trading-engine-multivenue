// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `hygiene.rs` (I17, I18, I24, I31, I32, I37, §3.6).

mod common;

use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace() -> PathBuf {
    crate_dir().join("../..")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if name == "target" || name == ".git" {
            continue;
        }
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn src_files() -> Vec<PathBuf> {
    let mut v = Vec::new();
    walk(&crate_dir().join("src"), &mut v);
    v.retain(|p| p.extension().is_some_and(|x| x == "rs"));
    v.sort();
    v
}

/// The code lines of a source (comment lines dropped).
fn code_lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .map(str::to_string)
        .collect()
}

/// `(section, key)` of every dependency entry of a Cargo.toml.
fn dep_keys(text: &str) -> Vec<(String, String)> {
    let mut sec = String::new();
    let mut out = Vec::new();
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            sec = t.trim_matches(|c| c == '[' || c == ']').to_string();
            continue;
        }
        if sec.contains("dependencies") {
            if let Some((k, _)) = t.split_once('=') {
                let k = k.trim();
                if !k.is_empty() && !k.starts_with('#') {
                    out.push((sec.clone(), k.to_string()));
                }
            }
        }
    }
    out
}

#[test]
fn hygiene_dependency_set_is_exact() {
    let text = std::fs::read_to_string(crate_dir().join("Cargo.toml")).unwrap();
    let keys = dep_keys(&text);
    let mut deps: Vec<&str> = keys
        .iter()
        .filter(|(s, _)| s == "dependencies")
        .map(|(_, k)| k.as_str())
        .collect();
    deps.sort_unstable();
    assert_eq!(deps, ["core-crypto", "pine-ir", "pine-lang", "pine-spec"]);
    for (s, k) in &keys {
        if s == "dev-dependencies" {
            assert_eq!(k, "proptest");
        }
        assert_ne!(s, "build-dependencies");
    }
}

#[test]
fn hygiene_no_crate_depends_on_pine_corpus() {
    let mut files = Vec::new();
    walk(&workspace(), &mut files);
    for f in files {
        if f.file_name().is_none_or(|n| n != "Cargo.toml") {
            continue;
        }
        if f.starts_with(crate_dir()) {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        for (s, k) in dep_keys(&text) {
            assert_ne!(
                k,
                "pine-corpus",
                "{} [{s}] depends on pine-corpus",
                f.display()
            );
        }
    }
}

#[test]
fn hygiene_corpus_inside_a_work_tree_is_refused() {
    let t = common::Tmp::new("worktree");
    std::fs::create_dir_all(t.path().join(".git")).unwrap();
    let root = t.path().join("a/b/corpus");
    std::fs::create_dir_all(&root).unwrap();
    assert!(matches!(
        pine_corpus::layout::Corpus::open(&root),
        Err(pine_corpus::CorpusError::InsideWorkTree { .. })
    ));
    // A linked worktree's `.git` is a file: refused too.
    let t2 = common::Tmp::new("worktree-file");
    std::fs::write(t2.path().join(".git"), "gitdir: elsewhere\n").unwrap();
    assert!(pine_corpus::layout::Corpus::open(t2.path()).is_err());
}

#[test]
fn hygiene_adapters_confine_upstream_names() {
    for f in src_files() {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let code = code_lines(&f).join("\n");
        if name != "spec_view.rs" {
            assert!(!code.contains("pine_spec::"), "{name} names pine_spec");
        }
        let n = code.matches("pine_lang::").count();
        match name.as_str() {
            "ast_view.rs" => {}
            "scan.rs" => {
                assert_eq!(n, 1, "scan.rs: only the one parse call");
                assert!(code.contains("pine_lang::parse("));
            }
            _ => assert_eq!(n, 0, "{name} names pine_lang"),
        }
    }
}

#[test]
fn hygiene_no_clock_no_unsafe_no_dyn() {
    for f in src_files() {
        for l in code_lines(&f) {
            for bad in [
                "SystemTime::now",
                "Instant::now",
                "UNIX_EPOCH",
                "unsafe {",
                "unsafe fn",
                "unsafe impl",
                "dyn ",
            ] {
                assert!(!l.contains(bad), "{}: {l}", f.display());
            }
        }
    }
}

#[test]
fn hygiene_no_std_net() {
    // I37: the crate opens no socket.
    for f in src_files() {
        for l in code_lines(&f) {
            for bad in [
                "std::net",
                "TcpStream",
                "TcpListener",
                "UdpSocket",
                "ToSocketAddrs",
            ] {
                assert!(!l.contains(bad), "{}: {l}", f.display());
            }
        }
    }
}

#[test]
fn hygiene_every_source_file_carries_the_copy_doctrine() {
    for f in src_files() {
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(
            text.lines()
                .take(60)
                .any(|l| l.starts_with("//! COPY-DOCTRINE:")),
            "{}",
            f.display()
        );
    }
}

#[test]
fn hygiene_crate_never_names_the_crawler() {
    let needle = ["tools", "_tvh", "arvest"].concat();
    let mut files = Vec::new();
    walk(&crate_dir(), &mut files);
    for f in files {
        let text = std::fs::read(&f).unwrap();
        let text = String::from_utf8_lossy(&text);
        assert!(!text.contains(&needle), "{}", f.display());
    }
}

fn container(rel: &str) -> bool {
    let comps: Vec<&str> = rel.split('/').collect();
    let (dirs, name) = comps.split_at(comps.len() - 1);
    let name = name[0];
    dirs.contains(&"corpus")
        || name.ends_with(".html.zst")
        || name == "endpoints.json"
        || name == "lib.lock"
        || name == "manual.tsv"
        || name.strip_suffix(".jsonl").is_some_and(|s| {
            [
                "index",
                "libs",
                "dedup",
                "builtins",
                "buckets",
                "topn-strategies",
                "topn-indicators",
                "libs-want",
                "fetchlog",
            ]
            .contains(&s)
        })
}

#[test]
fn hygiene_fixture_names_avoid_the_container_patterns() {
    let mut files = Vec::new();
    walk(&crate_dir(), &mut files);
    for f in files {
        let rel = f
            .strip_prefix(workspace().canonicalize().unwrap_or_else(|_| workspace()))
            .unwrap_or_else(|_| f.strip_prefix(workspace()).unwrap_or(&f))
            .to_string_lossy()
            .replace('\\', "/");
        assert!(!container(&rel), "{rel}");
    }
    assert!(container("x/manual.tsv"));
    assert!(container("corpus/x"));
    assert!(!container(
        "crates/pine-corpus/tests/fixtures/index.fixture.jsonl"
    ));
}

#[test]
fn hygiene_fixtures_are_ours() {
    let mut files = Vec::new();
    walk(&common::fixtures(), &mut files);
    let mut n = 0;
    for f in files {
        if f.extension().is_none_or(|x| x != "pine") {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        let l: Vec<&str> = text.lines().take(4).collect();
        assert!(l[0].starts_with("//@version="), "{}", f.display());
        assert_eq!(
            l[1],
            "// SPDX-License-Identifier: Apache-2.0",
            "{}",
            f.display()
        );
        assert_eq!(
            l[2],
            "// Copyright 2026 Anton (darkcite)",
            "{}",
            f.display()
        );
        assert_eq!(l[3], "// OUR CODE — not harvested", "{}", f.display());
        n += 1;
    }
    assert!(n >= 40, "{n} fixtures");
}

#[test]
fn hygiene_binary_reads_only_its_corpus() {
    let (_t, c) = common::corpus(
        "bin",
        &[common::S::new("a", &common::fixture("fork_b01.pine"))],
        &[],
    );
    let cwd = common::Tmp::new("bin-cwd");
    let before: Vec<PathBuf> = {
        let mut v = Vec::new();
        walk(c.root(), &mut v);
        v.sort();
        v
    };
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_pine-corpus"))
        .args(["build", "--corpus"])
        .arg(c.root())
        .args([
            "--tiers",
            "0b0111",
            "--n-strategies",
            "all",
            "--n-indicators",
            "10",
        ])
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );
    let mut after = Vec::new();
    walk(c.root(), &mut after);
    after.sort();
    for p in &after {
        if !before.contains(p) {
            assert!(p.starts_with(c.root().join("out")), "{}", p.display());
        }
    }
    assert_eq!(std::fs::read_dir(cwd.path()).unwrap().count(), 0);
    // Usage errors exit 2; a corpus inside a work tree exits 3.
    let u = std::process::Command::new(env!("CARGO_BIN_EXE_pine-corpus"))
        .arg("build")
        .output()
        .unwrap();
    assert_eq!(u.status.code(), Some(2));
}
