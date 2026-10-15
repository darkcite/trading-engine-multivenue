// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `build/json.rs` (impl-00 §4.4, §7): well-formed and malformed cases with exact offsets, the
//! int/float split, and the no-panic property (I36).

#[path = "../build/json.rs"]
mod json;

use json::{parse, Node, Val};
use std::borrow::Cow;

fn n(off: u32, v: Val<'_>) -> Node<'_> {
    Node { off, v }
}

fn s(x: &str) -> Val<'_> {
    Val::Str(Cow::Borrowed(x))
}

fn o(x: &str) -> Val<'static> {
    Val::Str(Cow::Owned(x.to_string()))
}

#[test]
fn json_parses_wellformed_cases() {
    let deep = format!("{}{}", "[".repeat(64), "]".repeat(64));
    let cases: Vec<(&[u8], Node<'_>)> = vec![
        (b"null", n(0, Val::Null)),
        (b" true ", n(1, Val::Bool(true))),
        (b"false", n(0, Val::Bool(false))),
        (b"0", n(0, Val::Int(0))),
        (b"-0", n(0, Val::Int(0))),
        (b"-0.0", n(0, Val::Num(-0.0))),
        (b"123456789", n(0, Val::Int(123_456_789))),
        (b"-9223372036854775808", n(0, Val::Int(i64::MIN))),
        (b"1.5e3", n(0, Val::Num(1500.0))),
        (b"2E-2", n(0, Val::Num(0.02))),
        (b"\"abc\"", n(0, s("abc"))),
        (b"\"\"", n(0, s(""))),
        (b"\"a\\\"b\\\\c\\/d\"", n(0, o("a\"b\\c/d"))),
        (b"\"\\b\\f\\n\\r\\t\"", n(0, o("\u{8}\u{c}\n\r\t"))),
        (b"\"\\u0041\\u00e9\"", n(0, o("A\u{e9}"))),
        (b"\"\\ud83d\\ude00\"", n(0, o("\u{1F600}"))),
        ("\"h\u{e9}\"".as_bytes(), n(0, s("h\u{e9}"))),
        (b"[]", n(0, Val::Arr(vec![]))),
        (b"{}", n(0, Val::Obj(vec![]))),
        (
            b"[1, 2.0]",
            n(0, Val::Arr(vec![n(1, Val::Int(1)), n(4, Val::Num(2.0))])),
        ),
        (
            b"{\"a\": [true], \"b\": {}}",
            n(
                0,
                Val::Obj(vec![
                    (
                        Cow::Borrowed("a"),
                        n(6, Val::Arr(vec![n(7, Val::Bool(true))])),
                    ),
                    (Cow::Borrowed("b"), n(19, Val::Obj(vec![]))),
                ]),
            ),
        ),
        (b"\n\t\r [ null ]", n(4, Val::Arr(vec![n(6, Val::Null)]))),
        (
            b"{\"k\":\"v\"}",
            n(0, Val::Obj(vec![(Cow::Borrowed("k"), n(5, s("v")))])),
        ),
    ];
    assert_eq!(cases.len(), 23);
    for (src, want) in &cases {
        let got =
            parse(src).unwrap_or_else(|e| panic!("{:?}: {e:?}", String::from_utf8_lossy(src)));
        assert_eq!(&got, want, "{:?}", String::from_utf8_lossy(src));
    }
    // The 24th: 64 nested arrays (the maximum depth).
    let got = parse(deep.as_bytes()).expect("depth 64");
    let mut cur = &got;
    let mut depth = 1;
    while let Val::Arr(a) = &cur.v {
        assert_eq!(cur.off as usize, depth - 1);
        match a.first() {
            Some(x) => {
                cur = x;
                depth += 1;
            }
            None => break,
        }
    }
    assert_eq!(depth, 64);
}

#[test]
fn json_rejects_malformed_cases() {
    let deep = format!("{}{}", "[".repeat(65), "]".repeat(65));
    let cases: Vec<(&[u8], u32)> = vec![
        (b"\"abc", 0),                // unterminated string
        (b"[1,]", 2),                 // trailing comma
        (b"{\"a\":1,}", 6),           // trailing comma in an object
        (b"\"\\x\"", 1),              // bad escape
        (b"\"\\ud800\"", 1),          // lone high surrogate
        (b"\"\\udc00\"", 1),          // lone low surrogate
        (b"01", 1),                   // leading zero
        (deep.as_bytes(), 64),        // depth 65
        (b"{\"a\":1,\"a\":2}", 7),    // duplicate key
        (b"\xEF\xBB\xBFnull", 0),     // BOM
        (b"\"a\x01\"", 2),            // control character
        (b"null x", 5),               // trailing content
        (b"", 0),                     // empty input
        (b"tru", 3),                  // truncated literal
        (b"1.", 2),                   // no digit after '.'
        (b"99999999999999999999", 0), // integer out of range
    ];
    assert_eq!(cases.len(), 16);
    for (src, off) in &cases {
        match parse(src) {
            Ok(v) => panic!("{:?} parsed as {v:?}", String::from_utf8_lossy(src)),
            Err(e) => assert_eq!(
                e.off,
                *off,
                "{:?}: {}",
                String::from_utf8_lossy(src),
                e.what
            ),
        }
    }
}

#[test]
fn json_distinguishes_int_and_float() {
    assert_eq!(parse(b"0").map(|n| n.v), Ok(Val::Int(0)));
    assert_eq!(parse(b"0.0").map(|n| n.v), Ok(Val::Num(0.0)));
    assert_eq!(parse(b"1e3").map(|n| n.v), Ok(Val::Num(1000.0)));
    assert_eq!(parse(b"-0").map(|n| n.v), Ok(Val::Int(0)));
    match parse(b"-0.0").map(|n| n.v) {
        Ok(Val::Num(f)) => assert!(f == 0.0 && f.is_sign_negative()),
        other => panic!("{other:?}"),
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4096))]
    #[test]
    fn json_never_panics_on_arbitrary_bytes(b in proptest::collection::vec(proptest::num::u8::ANY, 0..256)) {
        if let Err(e) = parse(&b) {
            proptest::prop_assert!(e.off as usize <= b.len());
        }
    }
}
