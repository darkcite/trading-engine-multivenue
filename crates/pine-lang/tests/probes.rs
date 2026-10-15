// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The syntax probes of §4.12 / §3.12: each pin's (or documented rule's) script, our result in
//! its `// OURS:` header, TradingView's in `// TV:` once captured.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use pine_ir::{FileId, SymTable};
use pine_lang::parse;

/// Every probe: (file name, text).
fn probes() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/probes");
    let mut out: Vec<(String, String)> = fs::read_dir(&dir)
        .expect("probes dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "pine"))
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (name, fs::read_to_string(&p).expect("read probe"))
        })
        .collect();
    out.sort();
    out
}

/// The value of header `// <key>: …`.
fn header<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let tag = format!("// {key}: ");
    text.lines().find_map(|l| l.strip_prefix(tag.as_str()))
}

/// Our result in the header's vocabulary: `accept`, or `reject <code>` (the one code reported,
/// possibly at several places).
fn ours(text: &str) -> String {
    let mut syms = SymTable::new();
    match parse(text, FileId::ROOT, &mut syms) {
        Ok(_) => "accept".to_string(),
        Err(d) => {
            let codes: BTreeSet<u16> = d.iter().map(|x| x.code.0).collect();
            let list: Vec<String> = codes.iter().map(u16::to_string).collect();
            format!("reject {}", list.join(","))
        }
    }
}

#[test]
fn probe_headers_match_our_parse() {
    let all = probes();
    assert_eq!(all.len(), 14);
    for (name, text) in &all {
        assert!(text.lines().count() <= 12, "{name} is longer than 12 lines");
        let want = header(text, "OURS").unwrap_or_else(|| panic!("{name}: no OURS"));
        assert_eq!(ours(text), want, "{name}");
        assert!(
            header(text, "PIN").is_some() != header(text, "RULE").is_some(),
            "{name}"
        );
    }
}

#[test]
fn probe_scripts_match_recorded_tradingview() {
    let mut pending = 0usize;
    for (name, text) in probes() {
        let tv = header(&text, "TV").unwrap_or_else(|| panic!("{name}: no TV"));
        if tv == "pending" {
            pending += 1;
            continue;
        }
        let accepted = tv == "accept";
        assert_eq!(
            ours(&text) == "accept",
            accepted,
            "{name}: TradingView says {tv}"
        );
    }
    println!("probes pending a TradingView capture: {pending}");
}

#[test]
fn every_pin_has_a_probe() {
    let pins: BTreeSet<String> = probes()
        .iter()
        .filter_map(|(_, t)| header(t, "PIN").map(str::to_string))
        .collect();
    for pin in [
        "pin:01-mixed-indent",
        "pin:01-bracket-wrap",
        "pin:01-v5-paren-wrap",
        "pin:01-string-line-wrap",
        "pin:01-version-form",
        "pin:01-version-conflict",
        "pin:01-local-type-decl",
        "pin:01-var-field",
        "pin:01-array-suffix-type",
    ] {
        assert!(pins.contains(pin), "{pin} has no probe");
    }
}
