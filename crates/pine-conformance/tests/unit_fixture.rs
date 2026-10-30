// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.3 — the `.fix` model, parser and renderer (I8), `dialect_text`, `is_capture_utc`.
//! The twelve files under `tests/fix/` are hand-written (ours, Apache-2.0, no TradingView data).

use std::path::PathBuf;

use pine_conformance::code;
use pine_conformance::fixture::{
    dialect_text, is_capture_utc, Dialects, Expect, Fixture, FixtureId, Group, ReadingBody,
};
use pine_ir::{DiagCode, Dialect, FaultKind};

fn fix_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fix")
}

fn load_all() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(fix_dir())
        .expect("tests/fix exists")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "fix"))
        .map(|p| {
            let t = std::fs::read_to_string(&p).expect("readable");
            (p.file_name().unwrap().to_string_lossy().into_owned(), t)
        })
        .collect();
    out.sort();
    out
}

fn text(name: &str) -> String {
    std::fs::read_to_string(fix_dir().join(name)).expect("fixture file")
}

fn parse(t: &str) -> Fixture {
    Fixture::parse(t, "mem.fix").unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn fixture_parse_render_round_trips() {
    let all = load_all();
    assert_eq!(all.len(), 12, "twelve hand-written .fix files");
    let mut kinds = std::collections::BTreeSet::new();
    for (name, t) in &all {
        let f = Fixture::parse(t, name).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(&f.render(), t, "{name}: render(parse(t)) != t");
        assert_eq!(
            Fixture::parse(&f.render(), name).unwrap(),
            f,
            "{name}: model round trip"
        );
        kinds.insert(match &f.expect {
            Expect::Pending => "pending",
            Expect::Series(_) => "series",
            Expect::Strategy {
                summary: Some(_), ..
            } => "strategy+summary",
            Expect::Strategy { summary: None, .. } => "strategy",
            Expect::Refuse { .. } => "refuse",
            Expect::Fault { .. } => "fault",
            Expect::Parse { accept: true } => "parse-accept",
            Expect::Parse { accept: false } => "parse-reject",
            Expect::Relation(_) => "relation",
        });
        for r in &f.readings {
            kinds.insert(match r.body {
                ReadingBody::Rows(_) => "reading-rows",
                ReadingBody::Refuse(_) => "reading-refuse",
                ReadingBody::Fault { .. } => "reading-fault",
                ReadingBody::Relation(_) => "reading-relation",
            });
        }
    }
    let want = [
        "pending",
        "series",
        "strategy+summary",
        "strategy",
        "refuse",
        "fault",
        "parse-accept",
        "parse-reject",
        "relation",
        "reading-rows",
        "reading-refuse",
        "reading-fault",
        "reading-relation",
    ];
    for k in want {
        assert!(kinds.contains(k), "no hand-written fixture covers `{k}`");
    }
}

#[test]
fn fixture_parse_rejects_unknown_directive() {
    let t = text("A001.fix").replacen("script\t", "colour\tred\nscript\t", 1);
    let e = Fixture::parse(&t, "x.fix").unwrap_err();
    assert!(e.what.contains("unknown directive `colour`"), "{e}");
    assert_eq!(e.line, 5);
    // A known directive out of its fixed place is refused too.
    let t = text("A001.fix")
        .replacen("mode\tbar_close\n", "", 1)
        .replacen("dialects\t56\n", "mode\tbar_close\ndialects\t56\n", 1);
    assert!(Fixture::parse(&t, "x.fix")
        .unwrap_err()
        .what
        .contains("out of order"));
}

#[test]
fn fixture_parse_rejects_row_key_mismatch() {
    // A key that disagrees with the row's position (a truncated or reordered paste).
    let t = text("B011.fix").replacen("18\t0\t1285.25\n", "16\t0\t1285.25\n", 1);
    let e = Fixture::parse(&t, "B011.fix").unwrap_err();
    assert!(e.what.contains("does not match its position"), "{e}");
    // A dropped row: the window declares three.
    let t = text("B011.fix").replacen("18\t0\t1285.25\n", "", 1);
    assert!(Fixture::parse(&t, "B011.fix").is_err());
    // Align time: keys must strictly increase and stay inside the window.
    let t = text("E001.fix").replacen("1772672400000\t1\n", "1772668800000\t1\n", 1);
    assert!(Fixture::parse(&t, "E001.fix").is_err());
    let t = text("E001.fix").replacen("1772676000000\t-1\n", "1788220800000\t-1\n", 1);
    assert!(Fixture::parse(&t, "E001.fix").is_err());
}

#[test]
fn fixture_parse_rejects_schema_1() {
    let t = text("A001.fix").replacen("fix\t2\n", "fix\t1\n", 1);
    let e = Fixture::parse(&t, "x.fix").unwrap_err();
    assert_eq!(e.line, 1);
    assert!(e.what.contains("schema 1"), "{e}");
}

#[test]
fn fixture_parse_rejects_a_trade_row_of_13_fields() {
    let t = text("E001.fix").replacen("\t75.3\t44\n", "\t75.3\n", 1);
    let e = Fixture::parse(&t, "E001.fix").unwrap_err();
    assert!(e.what.contains("14 fields, found 13"), "{e}");
    // An open trade carries `-` in pnl and cum_pnl.
    let t = text("E001.fix").replacen("\t0.5\t-\t-\t", "\t0.5\t1.0\t-\t", 1);
    assert!(Fixture::parse(&t, "E001.fix").is_err());
}

#[test]
fn fixture_id_grammar() {
    for ok in ["A001", "F098", "Z019", "B000", "E999"] {
        let id = FixtureId::parse(ok).unwrap();
        assert_eq!(id.as_str(), ok);
    }
    for bad in ["G001", "a001", "A01", "A0011", "A0x1", "", "Z 01"] {
        assert!(FixtureId::parse(bad).is_none(), "{bad}");
    }
    let id = FixtureId::parse("E121").unwrap();
    assert_eq!((id.group(), id.number()), (Group::E, 121));
    assert_eq!(FixtureId::parse("Z007").unwrap().group(), Group::Z);
    // The header's group must agree with the id.
    let t = text("A001.fix").replacen("group\tA\n", "group\tB\n", 1);
    assert!(Fixture::parse(&t, "x.fix").is_err());
}

const SRC6: &str = "// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n\
//@version=6\nindicator(\"x\")\n// @version=5 is mentioned here\nplot(1, \"f_A001_val\")\n";

#[test]
fn dialect_text_rewrites_only_the_version_line() {
    let same = dialect_text(SRC6, Dialects::BOTH, Dialect::V6).unwrap();
    assert!(matches!(same, std::borrow::Cow::Borrowed(_)));
    assert_eq!(same, SRC6);
    let v5 = dialect_text(SRC6, Dialects::BOTH, Dialect::V5).unwrap();
    assert_eq!(v5, SRC6.replacen("//@version=6\n", "//@version=5\n", 1));
    let a: Vec<&str> = SRC6.lines().collect();
    let b: Vec<&str> = v5.lines().collect();
    let differ: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
    assert_eq!(differ, vec![2]);
}

#[test]
fn dialect_text_refuses_a_single_dialect_mismatch() {
    // A single-dialect text naming the other dialect.
    let e = dialect_text(SRC6, Dialects::V5, Dialect::V5).unwrap_err();
    assert_eq!(e.line, 3);
    // A dialect outside the declared set.
    assert!(dialect_text(SRC6, Dialects::V6, Dialect::V5).is_err());
    // Missing, malformed or doubled version line.
    assert!(dialect_text("plot(1)\n", Dialects::BOTH, Dialect::V6).is_err());
    assert!(dialect_text("//@version=4\n", Dialects::BOTH, Dialect::V6).is_err());
    assert!(dialect_text("//@version= 6\n", Dialects::BOTH, Dialect::V6).is_err());
    assert!(dialect_text("//@version=6\n//@version=6\n", Dialects::BOTH, Dialect::V6).is_err());
    assert!(dialect_text(SRC6, Dialects::V6, Dialect::V6).is_ok());
}

#[test]
fn fixture_error_is_7003() {
    let e = Fixture::parse("fix\t2\n", "short.fix").unwrap_err();
    assert_eq!(e.code(), DiagCode(7003));
    assert_eq!(e.code(), code::FIXTURE_PARSE);
    assert_eq!(e.path, "short.fix");
    assert!(e.to_string().ends_with("(7003)"));
    // Never panics, whatever the bytes.
    for t in [
        "",
        "\n",
        "fix",
        "fix\t2",
        "\t\n",
        "fix\t2\r\n",
        "fix \t2\n",
        "fix\t\t2\n",
    ] {
        assert_eq!(Fixture::parse(t, "x").unwrap_err().code(), DiagCode(7003));
    }
    let full = text("E001.fix");
    for cut in 0..full.len() {
        if full.is_char_boundary(cut) && cut < full.len() {
            let _ = Fixture::parse(&full[..cut], "cut.fix");
        }
    }
}

#[test]
fn fixture_capture_utc_is_the_minute_form() {
    for ok in [
        "2026-11-06T09:14Z",
        "2028-02-29T00:00Z",
        "2000-02-29T23:59Z",
        "2026-12-31T23:59Z",
    ] {
        assert!(is_capture_utc(ok), "{ok}");
    }
    for bad in [
        "2026-11-06",
        "2026-11-06T09:14:00Z",
        "2026-11-06T09:14+00:00",
        "2026-11-06T09:14",
        "2026-02-30T00:00Z",
        "2026-11-06T24:00Z",
        "2026-11-06T09:60Z",
        "2026-13-01T00:00Z",
        "2026-00-01T00:00Z",
        "2026-11-00T00:00Z",
        "2100-02-29T00:00Z",
        "2026-11-06t09:14Z",
        "2026-1-06T09:14Z ",
        "",
    ] {
        assert!(!is_capture_utc(bad), "{bad}");
    }
    // The .fix parser holds `captured` to the same form.
    let t = text("B011.fix").replacen("2026-11-06T09:14Z", "2026-11-06T09:14:00Z", 1);
    assert!(Fixture::parse(&t, "B011.fix")
        .unwrap_err()
        .what
        .contains("captured"));
}

#[test]
fn fixture_reading_bodies_round_trip() {
    let b = parse(&text("B011.fix"));
    assert!(matches!(&b.readings[0].body, ReadingBody::Rows(r) if r.len() == 3 && r[0].key == 17));
    let f079 = parse(&text("F079.fix"));
    assert_eq!(f079.readings[1].body, ReadingBody::Refuse(DiagCode(3012)));
    let f083 = parse(&text("F083.fix"));
    assert_eq!(
        f083.readings[1].body,
        ReadingBody::Fault {
            kind: FaultKind::CollectionBounds,
            row: 0
        }
    );
    let c = parse(&text("C039.fix"));
    assert_eq!(
        c.readings[0].body,
        ReadingBody::Relation(vec![("f_C039_var".into(), 3), ("f_C039_rt".into(), -2)])
    );
    for f in [&b, &f079, &f083, &c] {
        let again = parse(&f.render());
        assert_eq!(&again, f);
    }
    // A reading in a fixture without a pin is refused; so is an unknown relation title.
    let t = text("A001.fix") + "reading\tx\nend\n";
    assert!(Fixture::parse(&t, "x.fix").is_err());
    let t = text("C039.fix").replacen("f_C039_var\t3\n", "f_C039_zz\t3\n", 1);
    assert!(Fixture::parse(&t, "C039.fix").is_err());
    // A refuse reading outside the load-time range is refused.
    let t = text("F079.fix").replacen("refuse\t3012", "refuse\t7003", 1);
    assert!(Fixture::parse(&t, "F079.fix").is_err());
}
