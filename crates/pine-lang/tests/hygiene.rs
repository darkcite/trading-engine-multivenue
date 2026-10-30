// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Mechanical hygiene of the crate (§5 I4, I12, I14, I16, I22). The grep tests read `src/` from
//! `CARGO_MANIFEST_DIR`, skip `src/tests/`, and are blunt on purpose.

use std::fs;
use std::path::{Path, PathBuf};

use pine_ir::DiagCode;
use pine_lang::code;

/// Every `.rs` under `src/`, except `src/tests/`, with its text.
fn sources() -> Vec<(PathBuf, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).expect("read src dir") {
            let p = e.expect("dir entry").path();
            if p.is_dir() {
                if p != root.join("tests") {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = fs::read_to_string(&p).expect("read source");
                out.push((p, text));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn diag_codes_are_dense_documented_and_unique() {
    assert_eq!(code::ALL.len(), 37);
    let mut prev = DiagCode(0);
    for c in code::ALL {
        assert!(c > prev, "{c:?} not strictly ascending");
        prev = c;
        assert!(
            (1000..=2999).contains(&c.0) || c.0 == 3001,
            "{c:?} out of range"
        );
        assert!(code::slug(c).is_some(), "{c:?} has no slug");
        assert!(!code::RETIRED.contains(&c.0), "{c:?} is retired");
    }
    for r in code::RETIRED {
        assert!(code::slug(DiagCode(r)).is_none(), "retired {r} has a slug");
    }
}

#[test]
fn crate_depends_on_pine_ir_only() {
    let toml = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("read Cargo.toml");
    let deps = toml
        .split("[dependencies]")
        .nth(1)
        .expect("a [dependencies] table")
        .split("\n[")
        .next()
        .expect("table body");
    let names: Vec<&str> = deps
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split(['=', ' ']).next().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["pine-ir"]);
}

/// The licensed copy sites: the literal scratch (§4.13) and the parser's node-id list relocation
/// (§4.9, lead ruling 2026-10-15, pending the operator's confirmation).
const COPY_SITES: [&str; 2] = ["lex/literal.rs", "ast/build.rs"];

#[test]
fn copy_markers_only_at_licensed_sites() {
    let mut copies = Vec::new();
    let mut doctrine = Vec::new();
    for (p, text) in sources() {
        let name = p.to_string_lossy().into_owned();
        copies.extend(text.matches("// COPY:").map(|_| name.clone()));
        doctrine.extend(text.matches("COPY-DOCTRINE:").map(|_| name.clone()));
    }
    assert_eq!(copies.len(), COPY_SITES.len(), "{copies:?}");
    for site in COPY_SITES {
        assert_eq!(
            copies.iter().filter(|c| c.ends_with(site)).count(),
            1,
            "{copies:?}"
        );
    }
    assert_eq!(doctrine.len(), 1, "{doctrine:?}");
    assert!(doctrine[0].ends_with("diag.rs"));
}

/// The code part of a line (blunt: everything before the first `//`).
fn code_of(line: &str) -> &str {
    line.split("//").next().unwrap_or("")
}

#[test]
fn owned_strings_only_in_diag_rs() {
    let banned = ["String", "format!", "to_string", "to_owned", "write!"];
    for (p, text) in sources() {
        if p.ends_with("diag.rs") {
            continue;
        }
        for line in text.lines() {
            let code = code_of(line);
            for b in banned {
                assert!(!code.contains(b), "{p:?}: {line}");
            }
        }
    }
}

#[test]
fn policy_literals_only_in_policy_rs() {
    for (p, text) in sources() {
        if p.ends_with("policy.rs") {
            continue;
        }
        for line in text.lines() {
            assert!(!code_of(line).contains("SyntaxPolicy {"), "{p:?}: {line}");
        }
    }
}

#[test]
fn no_dyn_no_unsafe_in_src() {
    for (p, text) in sources() {
        for line in text.lines() {
            let code = code_of(line);
            assert!(!code.contains("dyn "), "{p:?}: {line}");
            assert!(!code.contains("unsafe "), "{p:?}: {line}");
        }
    }
    let lib = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .expect("read lib.rs");
    assert!(lib.contains("#![forbid(unsafe_code)]"));
}

#[test]
fn syntax_rules_are_isolated() {
    for (p, text) in sources() {
        let in_layout = p.ends_with("lex/layout.rs");
        for line in text.lines() {
            let code = code_of(line);
            if code.contains("const INDENT_UNIT_SPACES") || code.contains("fn classify_line_start")
            {
                assert!(in_layout, "{p:?}: {line}");
            }
            if !in_layout {
                assert!(!code.contains("% 4"), "{p:?}: {line}");
            }
        }
    }
}

#[test]
fn pod_sizes_are_pinned() {
    let all: String = sources().into_iter().map(|(_, t)| t).collect();
    for pin in [
        "size_of::<Token>() == 16",
        "size_of::<RawNode>() == 32",
        "size_of::<DocRun>() == 12",
        "size_of::<Annotation>() == 20",
    ] {
        assert!(all.contains(pin), "missing const assert: {pin}");
    }
}
