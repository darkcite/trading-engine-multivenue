// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The syntax probes of §4.12 / §3.12: each pin's (or documented rule's) script, our result in
//! its `// OURS:` header, TradingView's in `// TV:` once captured.

use std::collections::{BTreeMap, BTreeSet};
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

/// TradingView's compile outcome of each probe as captured into its group-Z fixture (impl-06
/// §3.12): probe file name → accepted. The capture lands in `tests/conformance/fixtures/Z/*.fix`
/// (`script` names the probe, `parse accept|reject`), not in the probe's `// TV:` header — the
/// fixture's `script_sha256` pins the probe's bytes (I2), so the header stays `pending`.
fn captured_verdicts() -> BTreeMap<String, bool> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance/fixtures/Z");
    let mut out = BTreeMap::new();
    let Ok(rd) = fs::read_dir(&dir) else {
        return out;
    };
    for e in rd {
        let p = e.expect("entry").path();
        if p.extension().is_none_or(|x| x != "fix") {
            continue;
        }
        let text = fs::read_to_string(&p).expect("read fix");
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('\t')))
        };
        let (Some(script), Some(parse)) = (field("script"), field("parse")) else {
            continue;
        };
        if let Some(name) = script.strip_prefix("crates/pine-lang/tests/probes/") {
            out.insert(name.to_string(), parse == "accept");
        }
    }
    out
}

/// Our result, which must equal the `// OURS:` header — except where the probe's group-Z capture
/// overturned that header (R-P1/R-P20: a `POLICY` field flipped to TradingView's outcome, CI-023:
/// p01 p06 p07 p08 p14); there our result must be the captured one.
#[test]
fn probe_headers_match_our_parse() {
    let all = probes();
    assert_eq!(all.len(), 14);
    let tv = captured_verdicts();
    for (name, text) in &all {
        assert!(text.lines().count() <= 12, "{name} is longer than 12 lines");
        let want = header(text, "OURS").unwrap_or_else(|| panic!("{name}: no OURS"));
        let got = ours(text);
        match tv.get(name.as_str()) {
            Some(&acc) if (want == "accept") != acc => {
                assert_eq!(
                    got == "accept",
                    acc,
                    "{name}: TradingView's capture, we say {got}"
                )
            }
            _ => assert_eq!(got, want, "{name}"),
        }
        assert!(
            header(text, "PIN").is_some() != header(text, "RULE").is_some(),
            "{name}"
        );
    }
}

#[test]
fn probe_scripts_match_recorded_tradingview() {
    let mut pending = 0usize;
    let captured = captured_verdicts();
    for (name, text) in probes() {
        let tv = header(&text, "TV").unwrap_or_else(|| panic!("{name}: no TV"));
        let accepted = match (tv, captured.get(name.as_str())) {
            (_, Some(&acc)) => acc,
            ("pending", None) => {
                pending += 1;
                continue;
            }
            (tv, None) => tv == "accept",
        };
        assert_eq!(
            ours(&text) == "accept",
            accepted,
            "{name}: TradingView says {}",
            if accepted { "accept" } else { "reject" }
        );
    }
    println!("probes pending a TradingView capture: {pending}");
}

/// The probe `name`'s text.
fn probe(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/probes")
        .join(name);
    fs::read_to_string(p).expect("read probe")
}

/// pin:01-mixed-indent `TabIsUnit` — TradingView accepts p01 (Z001): a tab then four spaces is the
/// third level, inside the nested `if`.
#[test]
fn parse_accepts_z001_mixed_indent() {
    assert_eq!(ours(&probe("p01_mixed_indent.pine")), "accept");
}

/// pin:01-string-line-wrap — TradingView accepts p06 (Z006).
#[test]
fn parse_accepts_z006_string_line_wrap() {
    assert_eq!(ours(&probe("p06_string_line_wrap.pine")), "accept");
}

/// pin:01-version-form — TradingView accepts p07 (Z007): `//@version = 6`.
#[test]
fn parse_accepts_z007_version_spaced() {
    let text = probe("p07_version_spaced.pine");
    assert_eq!(ours(&text), "accept");
    assert!(matches!(
        pine_lang::version_of(&text, FileId::ROOT),
        pine_lang::VersionProbe::Supported {
            dialect: pine_ir::Dialect::V6,
            ..
        }
    ));
}

/// pin:01-version-conflict — TradingView accepts p08 (Z008): `=5` then `=6`; the first decides.
#[test]
fn parse_accepts_z008_version_conflict() {
    let text = probe("p08_version_conflict.pine");
    assert_eq!(ours(&text), "accept");
    assert!(matches!(
        pine_lang::version_of(&text, FileId::ROOT),
        pine_lang::VersionProbe::Supported {
            dialect: pine_ir::Dialect::V5,
            ..
        }
    ));
}

/// pin:01-first-line-indent — TradingView accepts p14 (Z014): ` indicator("p14")` at column 1 is
/// the first code line; it continues nothing and starts the first statement. A later indented
/// line stays a wrap (R3), so `x = 1` then ` y = 2` is still refused.
#[test]
fn parse_accepts_z014_global_leading_space() {
    assert_eq!(ours(&probe("p14_global_leading_space.pine")), "accept");
    assert_ne!(ours("//@version=6\nx = 1\n y = 2\n"), "accept");
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
