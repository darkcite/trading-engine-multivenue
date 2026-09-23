// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Mechanical laws over src/: no clock (P5), `dyn` only as `dyn BarCtx`, no
//! allocation verbs outside verify/ and testkit.rs, no iterator loops in hot files.

use std::fs;
use std::path::Path;

fn files(dir: &Path, out: &mut Vec<(String, String)>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files(&p, out);
        } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
            out.push((p.to_string_lossy().into_owned(), fs::read_to_string(&p).unwrap()));
        }
    }
}

fn code_lines(src: &str) -> impl Iterator<Item = (usize, &str)> {
    src.lines().enumerate().filter(|(_, l)| !l.trim_start().starts_with("//"))
}

#[test]
fn grep_no_clock_anywhere() {
    let mut v = Vec::new();
    files(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut v);
    for (f, s) in &v {
        for (n, l) in code_lines(s) {
            for bad in ["std::time", "Instant", "SystemTime", "clock_gettime", "catch_unwind"] {
                assert!(!l.contains(bad), "{f}:{}: clock or unwind `{bad}`", n + 1);
            }
        }
    }
}

#[test]
fn grep_dyn_only_for_barctx() {
    let mut v = Vec::new();
    files(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut v);
    for (f, s) in &v {
        for (n, l) in code_lines(s) {
            if let Some(i) = l.find("dyn ") {
                assert!(l[i..].starts_with("dyn BarCtx"), "{f}:{}: `dyn` outside BarCtx", n + 1);
            }
        }
    }
}

#[test]
fn grep_no_allocation_or_iterators_on_hot_files() {
    let mut v = Vec::new();
    files(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut v);
    for (f, s) in &v {
        if f.contains("/verify/") || f.ends_with("testkit.rs") {
            continue;
        }
        let body = s.split("#[cfg(test)]").next().unwrap();
        for (n, l) in code_lines(body) {
            for bad in ["Vec::", "vec!", "Box::new", "format!", "String", "to_string", ".collect", "to_vec", ".iter()", "(|", "|_|"] {
                assert!(!l.contains(bad), "{f}:{}: `{bad}` on a hot file", n + 1);
            }
            if f.ends_with("interp.rs") {
                assert!(!l.contains("?;") && !l.contains(")?"), "{f}:{}: `?` in the dispatch loop", n + 1);
            }
            assert!(!(l.contains("for ") && l.contains(" in ")), "{f}:{}: iterator loop on a hot file", n + 1);
        }
    }
}

#[test]
fn grep_no_touch_state() {
    let mut v = Vec::new();
    files(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut v);
    for (f, s) in &v {
        assert!(!s.contains("touch_state"), "{f}: `touch_state` must not exist (I18)");
    }
}
