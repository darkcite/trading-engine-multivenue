// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 T24 (§7.7): the grep gates as tests — I4, I5, I13, I30, I33 and the `live_bound`
//! literal (I8). Each scans the crate's `src/` (comments stripped; a file is read up to its
//! `#[cfg(test)]` tests module, which may allocate) and names the offending line.
//!
//! `PINE_GATES_SRC` points the gates at another copy of `src/` — how each gate was proven to fail
//! on a violating line in a scratch copy (T24's acceptance) without touching the crate.

use std::path::PathBuf;

/// The hot modules (I4): nothing a callback reaches allocates, closes over, iterates or `dyn`s.
const HOT: [&str; 15] = [
    "bars", "series", "ctx", "step", "coalesce", "clock", "oid", "host", "settle", "fills",
    "fault", "arm", "tape", "counters", "marks",
];

/// The boot-only modules (COPY-DOCTRINE in `lib.rs`): they may hold the venue's boot facts.
/// `bootrec` (R5) renders / parses the boot record — `reconcile_boot`'s values as a file line.
const BOOT_ONLY: [&str; 5] = ["boot", "bootrec", "load", "checks", "warm"];

fn src_dir() -> PathBuf {
    std::env::var_os("PINE_GATES_SRC").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        PathBuf::from,
    )
}

/// `(module, line number, code)` for every code line of `src/<module>.rs` before its tests
/// module, with `//` comments (doc comments included) cut off.
fn code(module: &str) -> Vec<(String, usize, String)> {
    let path = src_dir().join(format!("{module}.rs"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("#[cfg(test)]") {
            break;
        }
        let c = line.split("//").next().unwrap_or("");
        if !c.trim().is_empty() {
            out.push((module.to_string(), n + 1, c.to_string()));
        }
    }
    out
}

/// Every module of `src/`, by stem, sorted.
fn modules() -> Vec<String> {
    let mut m: Vec<String> = std::fs::read_dir(src_dir())
        .expect("src")
        .filter_map(|e| {
            let p = e.expect("entry").path();
            (p.extension().is_some_and(|x| x == "rs"))
                .then(|| p.file_stem().expect("stem").to_string_lossy().into_owned())
        })
        .collect();
    m.sort();
    m
}

fn words(c: &str) -> Vec<&str> {
    c.split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .filter(|w| !w.is_empty())
        .collect()
}

fn fail(gate: &str, hits: &[String]) {
    assert!(hits.is_empty(), "{gate}:\n{}", hits.join("\n"));
}

/// I4: no `Vec`, `String`, `format!`, `Box::new`, `.collect(`, `HashMap`, `BTreeMap`, closure,
/// `for … in ….iter()` or `dyn` (but `dyn BarCtx` in `ctx`) in a hot module.
#[test]
fn no_alloc_or_dyn_on_hot_modules() {
    let mut hits = Vec::new();
    for m in HOT {
        for (f, n, c) in code(m) {
            let w = words(&c);
            let tok = ["Vec", "String", "HashMap", "BTreeMap"]
                .into_iter()
                .find(|t| w.contains(t));
            let lit = ["format!", "Box::new", ".collect("]
                .into_iter()
                .find(|t| c.contains(t));
            let squeezed: String = c.chars().filter(|ch| !ch.is_whitespace()).collect();
            let closure = ["(|", ",|", "=|", "(move|", ",move|", "=move|"]
                .into_iter()
                .any(|t| squeezed.contains(t))
                || c.contains("move |")
                || c.contains("move||");
            let iter_for = w.first() == Some(&"for") && w.contains(&"in") && c.contains(".iter()");
            let dyn_bad = w.contains(&"dyn") && !(f == "ctx" && c.contains("dyn BarCtx"));
            if tok.is_some() || lit.is_some() || closure || iter_for || dyn_bad {
                hits.push(format!("{f}.rs:{n}: {}", c.trim()));
            }
        }
    }
    fail("I4 (hot modules)", &hits);
}

/// I5: `now_ns()` exactly once in `src/`, in `tape.rs`; no `std::time`, `Instant`, `SystemTime`
/// or `WallAnchor::now` anywhere; nothing outside `tape` builds or reads a `PopStamp`.
#[test]
fn one_clock_read_and_it_is_write_only() {
    let mut reads = Vec::new();
    let mut hits = Vec::new();
    for m in modules() {
        for (f, n, c) in code(&m) {
            if c.contains("now_ns()") {
                reads.push(format!("{f}.rs:{n}"));
            }
            if ["std::time", "Instant", "SystemTime", "WallAnchor::now"]
                .into_iter()
                .any(|b| c.contains(b))
            {
                hits.push(format!("{f}.rs:{n}: {}", c.trim()));
            }
            if f != "tape" && (c.contains("PopStamp(") || c.contains("PopStamp::take")) {
                hits.push(format!("{f}.rs:{n}: {}", c.trim()));
            }
        }
    }
    fail("I5 (clock)", &hits);
    assert_eq!(reads.len(), 1, "I5: exactly one clock read: {reads:?}");
    assert!(
        reads[0].starts_with("tape.rs:"),
        "I5: in tape.rs: {reads:?}"
    );
}

/// I13: no boundary arithmetic — every bar boundary is `core_time::barclock`'s. A `%`, a `/`,
/// `rem_euclid` or `div_euclid` on a bar time or timeframe, or a timeframe scaled to a length,
/// is refused anywhere in `src/`.
#[test]
fn no_boundary_arithmetic_outside_barclock() {
    const TIME: [&str; 8] = [
        "tf_s",
        "tf_ms",
        "timeframe_s",
        "open_ms",
        "close_ms",
        "t_ms",
        "bar_ms",
        "len_ms",
    ];
    let mut hits = Vec::new();
    for m in modules() {
        for (f, n, c) in code(&m) {
            let w = words(&c);
            let timed = TIME.into_iter().any(|t| w.contains(&t))
                || c.contains("60_000")
                || c.contains("86_400");
            let divides = c.contains('%')
                || c.contains("rem_euclid")
                || c.contains("div_euclid")
                || c.contains('/');
            let scales = w.iter().any(|x| x.starts_with("tf"))
                && c.contains('*')
                && (c.contains("1000") || c.contains("1_000"));
            if timed && divides || scales {
                hits.push(format!("{f}.rs:{n}: {}", c.trim()));
            }
        }
    }
    fail("I13 (boundary arithmetic)", &hits);
}

/// I30: the member keeps no position, average price or day notional of its own: no such
/// field in any run-time module (the boot-only `VenueBootState` carries the venue's).
#[test]
fn member_keeps_no_position_state() {
    let mut hits = Vec::new();
    for m in modules() {
        if BOOT_ONLY.contains(&m.as_str()) {
            continue;
        }
        for (f, n, c) in code(&m) {
            let t = c.trim();
            let t = t
                .strip_prefix("pub(crate) ")
                .or_else(|| t.strip_prefix("pub "))
                .unwrap_or(t);
            let Some((name, ty)) = t.split_once(':') else {
                continue;
            };
            let name = name.trim();
            let ty = ty.trim().trim_end_matches(',').trim();
            let field = !name.contains(' ')
                && !name.is_empty()
                && ["i64", "u64", "i128", "u128", "i32", "u32", "f64"].contains(&ty);
            if field
                && ["position", "avg_px", "day_notional"]
                    .into_iter()
                    .any(|b| name.contains(b))
            {
                hits.push(format!("{f}.rs:{n}: {t}"));
            }
        }
    }
    fail("I30 (position state)", &hits);
}

/// I33: `disabled` is never assigned `false` — the one `disabled: false` is the constructor in
/// `rt.rs`.
#[test]
fn disabled_is_never_cleared() {
    let mut inits = Vec::new();
    let mut hits = Vec::new();
    for m in modules() {
        for (f, n, c) in code(&m) {
            let l: String = c.chars().filter(|ch| !ch.is_whitespace()).collect();
            if l.contains("disabled=false") || l.contains("disabled=!") {
                hits.push(format!("{f}.rs:{n}: {}", c.trim()));
            }
            if l.starts_with("disabled:false") {
                inits.push(format!("{f}.rs"));
            }
        }
    }
    fail("I33 (disabled cleared)", &hits);
    assert_eq!(
        inits,
        vec!["rt.rs".to_string()],
        "I33: only the constructor"
    );
}

/// I8 (the `live_bound` literal): `live_bound: true` is the crate's only `live_bound`.
#[test]
fn every_compile_is_live_bound_literal() {
    let mut trues = 0;
    let mut hits = Vec::new();
    for m in modules() {
        for (f, n, c) in code(&m) {
            if c.contains("live_bound") {
                let l: String = c.chars().filter(|ch| !ch.is_whitespace()).collect();
                if l.contains("live_bound:true") {
                    trues += 1;
                } else {
                    hits.push(format!("{f}.rs:{n}: {}", c.trim()));
                }
            }
        }
    }
    fail("I8 (live_bound)", &hits);
    assert_eq!(trues, 1, "one CompileOpts literal, live_bound: true");
}
