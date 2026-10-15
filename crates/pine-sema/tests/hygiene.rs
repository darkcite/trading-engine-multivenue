// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.11.5 — source scans (I1, I2, I26).

use std::path::{Path, PathBuf};

const SPDX: &str = "// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n";

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("dir")
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

fn src_files() -> Vec<(PathBuf, String)> {
    let mut v = Vec::new();
    rs_files(&crate_dir().join("src"), &mut v);
    v.into_iter()
        .map(|p| {
            let t = std::fs::read_to_string(&p).expect("utf-8");
            (p, t)
        })
        .collect()
}

/// The names of a `[section]` table of `Cargo.toml`.
fn table_keys(manifest: &str, section: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t.split('#').next().unwrap().trim() == section;
            continue;
        }
        if inside && !t.is_empty() && !t.starts_with('#') {
            keys.push(t.split('=').next().unwrap().trim().to_owned());
        }
    }
    keys.sort();
    keys
}

#[test]
fn hygiene_dependencies_are_contract_section_2() {
    let m = std::fs::read_to_string(crate_dir().join("Cargo.toml")).unwrap();
    assert_eq!(
        table_keys(&m, "[dependencies]"),
        ["pine-ir", "pine-lang", "pine-spec", "pine-std"]
    );
    assert_eq!(
        table_keys(&m, "[dev-dependencies]"),
        ["pine-vm", "proptest"]
    );
    assert!(!m.contains("[build-dependencies]"));
}

#[test]
fn hygiene_copy_doctrine_headers() {
    let mut all = Vec::new();
    rs_files(&crate_dir().join("src"), &mut all);
    rs_files(&crate_dir().join("tests"), &mut all);
    for p in &all {
        let t = std::fs::read_to_string(p).unwrap();
        assert!(t.starts_with(SPDX), "{} lacks the SPDX record", p.display());
    }
    for (p, t) in src_files() {
        let head: String = t.lines().take(12).collect::<Vec<_>>().join("\n");
        assert!(
            head.contains("COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is"),
            "{} lacks the COPY-DOCTRINE header",
            p.display()
        );
    }
}

#[test]
fn hygiene_no_unsafe() {
    for (p, t) in src_files() {
        assert!(!t.contains(concat!("uns", "afe ")), "{}", p.display());
        assert!(!t.contains(concat!("uns", "afe{")), "{}", p.display());
    }
    let lib = std::fs::read_to_string(crate_dir().join("src/lib.rs")).unwrap();
    assert!(lib.contains("#![forbid(unsafe_code)]"));
}

#[test]
fn hygiene_no_clock() {
    for (p, t) in src_files() {
        for bad in [
            concat!("std::", "time"),
            concat!("Inst", "ant"),
            concat!("System", "Time"),
        ] {
            let mut rest = t.as_str();
            while let Some(i) = rest.find(bad) {
                let after = &rest[i + bad.len()..];
                // a whole word only: `Instantiation` is not a clock
                assert!(
                    after.starts_with(|c: char| c.is_ascii_alphanumeric()),
                    "{}: {bad}",
                    p.display()
                );
                rest = after;
            }
        }
    }
}

#[test]
fn hygiene_no_numeric_overload_ids() {
    for (p, t) in src_files() {
        let pat = concat!("Overload", "Id(");
        let mut rest = t.as_str();
        while let Some(i) = rest.find(pat) {
            let after = &rest[i + pat.len()..];
            assert!(
                !after.starts_with(|c: char| c.is_ascii_digit()),
                "{}",
                p.display()
            );
            rest = after;
        }
    }
}

#[test]
fn hygiene_no_eight_decimal_scale() {
    for (p, t) in src_files() {
        for bad in [
            concat!("100_000", "_000"),
            concat!("1000", "00000"),
            concat!("1e", "8"),
            concat!("1E", "8"),
        ] {
            assert!(!t.contains(bad), "{}: {bad}", p.display());
        }
    }
}

#[test]
fn hygiene_maps_are_never_iterated() {
    for (p, t) in src_files() {
        for (n, line) in t.lines().enumerate() {
            let decl =
                line.contains(concat!("Hash", "Map")) || line.contains(concat!("Hash", "Set"));
            if decl && line.contains("let ") {
                assert!(line.contains("// INDEX-ONLY"), "{}:{}", p.display(), n + 1);
            }
        }
    }
}
