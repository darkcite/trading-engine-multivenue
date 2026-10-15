// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The worked example of impl-00 §4.2 lowers to its stated rows and ids; every negative fixture
//! fails with the rule its file name carries and a byte offset (I37).

#[path = "../src/bf_lowering.rs"]
mod bf_lowering;
#[path = "../build/json.rs"]
mod json;
// Shared with build.rs, which uses every item; this test reaches only the lowering half.
#[allow(dead_code)]
#[path = "../build/schema.rs"]
mod schema;
#[path = "../build/validate.rs"]
mod validate;

use schema::SpecErr;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn run(src: &[u8]) -> Result<schema::Model, SpecErr> {
    let dom = json::parse(src).map_err(|e| SpecErr {
        off: e.off,
        rule: 2,
        what: e.what.into(),
    })?;
    let m = schema::lower(&dom)?;
    validate::check(&m)?;
    Ok(m)
}

#[test]
fn worked_example_lowers() {
    let src = std::fs::read(format!("{DIR}/worked.json")).expect("worked.json");
    let dom = json::parse(&src).expect("worked.json parses");
    let m = schema::lower(&dom).expect("worked.json lowers");
    let counts = [
        m.entries.len(),
        m.overloads.len(),
        m.params.len(),
        m.rets.len(),
        m.values.len(),
        m.switches.len(),
        m.deltas.len(),
    ];
    assert_eq!(counts, [15, 13, 37, 11, 6, 4, 4]);
    let ids: Vec<&str> = m.overloads.iter().map(|o| o.sym.as_str()).collect();
    let want = [
        "OP_ADD_STR",
        "MAP_NEW",
        "MATH_MAX_F",
        "MATH_MAX_I",
        "NZ_F",
        "NZ_I",
        "NZ_B",
        "REQUEST_SECURITY",
        "STRATEGY",
        "STRATEGY_ENTRY",
        "TA_HIGHEST",
        "TA_HIGHEST_SRC",
        "TA_SMA",
    ];
    assert_eq!(ids, want);
    // Every rule but R10 holds on it; R10 refuses its "§" (impl-00 open question, report).
    let ascii = String::from_utf8(src)
        .expect("utf-8")
        .replace('\u{a7}', "section");
    run(ascii.as_bytes()).expect("worked.json validates once ASCII");
}

#[test]
fn each_invalid_fixture_names_its_rule() {
    let mut names: Vec<String> = std::fs::read_dir(format!("{DIR}/invalid"))
        .expect("invalid/")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert!(names.len() >= 18, "{names:?}");
    for name in &names {
        let rule: u8 = name[1..3].parse().expect("r<NN>_ prefix");
        let src = std::fs::read(format!("{DIR}/invalid/{name}")).expect("fixture");
        match run(&src) {
            Ok(_) => panic!("{name}: accepted"),
            Err(e) => {
                let msg = e.to_string();
                assert_eq!(e.rule, rule, "{name}: {msg}");
                assert!(
                    msg.starts_with(&format!("{}: R{rule:02}: ", e.off)),
                    "{name}: {msg}"
                );
                assert!((e.off as usize) < src.len(), "{name}: {msg}");
            }
        }
    }
}
