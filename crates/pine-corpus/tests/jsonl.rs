// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `jsonl.rs`.

use pine_corpus::jsonl::{parse_line, read_lines, write_str, Line, Value};
use proptest::prelude::*;

#[test]
fn jsonl_roundtrips_every_value_kind() {
    let mut out = Vec::new();
    Line::begin(&mut out)
        .str("s", "x\u{e9}y")
        .i64("imin", i64::MIN)
        .i64("imax", i64::MAX)
        .u64("umax", u64::MAX)
        .bool("t", true)
        .bool("f", false)
        .null("n")
        .arr("a", &["p".to_string(), "q".to_string()])
        .arr("e", &[])
        .end();
    let r = parse_line(&out).unwrap();
    assert_eq!(r.get("schema"), Some(&Value::Int(1)));
    assert_eq!(r.get("s"), Some(&Value::Str("x\u{e9}y".to_string())));
    assert_eq!(r.get("imin"), Some(&Value::Int(i128::from(i64::MIN))));
    assert_eq!(r.get("imax"), Some(&Value::Int(i128::from(i64::MAX))));
    assert_eq!(r.get("umax"), Some(&Value::Int(i128::from(u64::MAX))));
    assert_eq!(r.get("t"), Some(&Value::Bool(true)));
    assert_eq!(r.get("f"), Some(&Value::Bool(false)));
    assert_eq!(r.get("n"), Some(&Value::Null));
    assert_eq!(
        r.get("a"),
        Some(&Value::Arr(vec!["p".to_string(), "q".to_string()]))
    );
    assert_eq!(r.get("e"), Some(&Value::Arr(vec![])));
}

#[test]
fn jsonl_escapes_control_characters_and_quotes() {
    let s = "a\"b\\c\nd\u{1}e\u{e9}";
    let mut out = Vec::new();
    write_str(&mut out, s);
    let text = String::from_utf8(out.clone()).unwrap();
    assert_eq!(text, "\"a\\\"b\\\\c\\nd\\u0001e\u{e9}\"");
    let mut line = b"{\"schema\":1,\"k\":".to_vec();
    line.extend_from_slice(&out);
    line.push(b'}');
    assert_eq!(
        parse_line(&line).unwrap().get("k"),
        Some(&Value::Str(s.to_string()))
    );
}

#[test]
fn jsonl_reader_refuses_floats_and_nested_objects() {
    assert!(parse_line(b"{\"schema\":1,\"x\":1.5}").is_err());
    assert!(parse_line(b"{\"schema\":1,\"x\":1e3}").is_err());
    assert!(parse_line(b"{\"schema\":1,\"x\":{\"y\":1}}").is_err());
    assert!(parse_line(b"{\"schema\":1,\"x\":[1,2]}").is_err());
}

#[test]
fn jsonl_reader_ignores_unknown_keys() {
    let mut out = Vec::new();
    pine_corpus::schema::WantRow {
        user: "u".into(),
        lib: "l".into(),
        ver: 1,
        depth: 1,
        dependents: 2,
    }
    .write(&mut out);
    let mut line = out[..out.len() - 2].to_vec();
    line.extend_from_slice(b",\"zzz\":\"new\"}\n");
    let r = pine_corpus::schema::WantRow::read(&line).unwrap();
    assert_eq!(r.dependents, 2);
}

#[test]
fn jsonl_reader_refuses_lines_over_one_mebibyte() {
    let mut line = b"{\"schema\":1,\"k\":\"".to_vec();
    line.resize(line.len() + (1 << 20), b'a');
    line.extend_from_slice(b"\"}");
    assert_eq!(parse_line(&line), Err("line over 1 MiB"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(debug_assertions) { 1000 } else { 10_000 }))]
    #[test]
    fn jsonl_reader_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = parse_line(&bytes);
        let _ = read_lines(&bytes);
    }
}
