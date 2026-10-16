// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.4 — the load-time services (T1–T4, T22's `tf_on_bar_clock`); no `pine-vm` needed.

use pine_ir::{color, input_flag, Dialect, InputDesc, InputKind, InputValue, Ty};
use pine_std::*;

fn tf_err(s: &str) -> bool {
    matches!(tf_seconds(s.as_bytes()), Err(e) if e.code == code::BAD_TIMEFRAME)
}

#[test]
fn tf_grammar_table() {
    let ok: [(&str, u32); 16] = [
        ("1", 60),
        ("60", 3600),
        ("240", 14_400),
        ("1440", 86_400),
        ("S", 1),
        ("1S", 1),
        ("5S", 5),
        ("45S", 45),
        ("D", 86_400),
        ("1D", 86_400),
        ("365D", 365 * 86_400),
        ("W", 604_800),
        ("52W", 52 * 604_800),
        ("M", MONTH_SORT_S),
        ("3M", 3 * MONTH_SORT_S),
        ("12M", 12 * MONTH_SORT_S),
    ];
    for (s, v) in ok {
        assert_eq!(tf_seconds(s.as_bytes()), Ok(Some(v)), "{s}");
    }
    assert_eq!(tf_seconds(b""), Ok(None));
    for s in [
        "1H", "10T", "20S", "0", "05", "1d", "366D", "1441", "53W", "13M", "1DD", "-1", "1 D", "T",
    ] {
        assert!(tf_err(s), "{s} must be refused");
    }
}

#[test]
fn tf_roundtrip() {
    let mut buf = [0u8; 8];
    let mut secs: Vec<u32> = (1..=1440).map(|m| m * 60).collect();
    secs.extend([1, 5, 10, 15, 30, 45]);
    secs.extend((1..=365).map(|d| d * 86_400));
    secs.extend((1..=52).map(|w| w * 604_800));
    secs.extend((1..=12).map(|m| m * MONTH_SORT_S));
    for s in secs {
        for d in [Dialect::V5, Dialect::V6] {
            let n = tf_string(s, d, &mut buf);
            assert!(n > 0, "{s}");
            assert_eq!(tf_seconds(&buf[..n]), Ok(Some(s)), "{s} {d:?}");
        }
    }
    assert_eq!(tf_string(0, Dialect::V6, &mut buf), 0);
    assert_eq!(tf_string(20, Dialect::V6, &mut buf), 0);
}

#[test]
fn tf_string_dialects() {
    let mut b = [0u8; 8];
    let mut s = |secs: u32, d: Dialect| -> String {
        let n = tf_string(secs, d, &mut b);
        String::from_utf8(b[..n].to_vec()).unwrap()
    };
    assert_eq!(s(86_400, Dialect::V5), "D");
    assert_eq!(s(86_400, Dialect::V6), "1D");
    assert_eq!(s(3600, Dialect::V5), "60");
    assert_eq!(s(3600, Dialect::V6), "60");
    assert_eq!(s(604_800, Dialect::V5), "W");
    assert_eq!(s(604_800, Dialect::V6), "1W");
    assert_eq!(s(1_209_600, Dialect::V6), "2W");
    assert_eq!(s(MONTH_SORT_S, Dialect::V5), "M");
    assert_eq!(s(1, Dialect::V5), "S");
    assert_eq!(s(1, Dialect::V6), "1S");
}

#[test]
fn tf_parts_decomposes() {
    assert_eq!(
        tf_parts(14_400),
        Some(TfParts {
            unit: TfUnit::Minute,
            mult: 240
        })
    );
    assert_eq!(
        tf_parts(3 * MONTH_SORT_S),
        Some(TfParts {
            unit: TfUnit::Month,
            mult: 3
        })
    );
    assert_eq!(
        tf_parts(30),
        Some(TfParts {
            unit: TfUnit::Second,
            mult: 30
        })
    );
    assert_eq!(tf_parts(0), None);
}

#[test]
fn tf_on_bar_clock_follows_barclock() {
    for s in 0..=(13 * MONTH_SORT_S / 60) {
        for v in [s, s * 60] {
            let ok = core_time::barclock::is_allowed(v);
            match tf_on_bar_clock(v) {
                Ok(x) => assert!(ok && x == v),
                Err(e) => assert!(!ok && e.code == code::BAD_TIMEFRAME),
            }
        }
    }
}

#[test]
fn month_key_matches_barclock() {
    assert_eq!(MONTH_SORT_S, core_time::barclock::TF_MONTH_S);
}

#[test]
fn session_grammar_table() {
    let r = |a: u16, b: u16| SessionRange {
        start_min: a,
        end_min: b,
    };
    let s = session_parse(b"0930-1600").unwrap();
    assert_eq!((s.n, s.days, s.ranges[0]), (1, 0x7F, r(570, 960)));
    let s = session_parse(b"1700-0900:23456").unwrap();
    assert_eq!((s.n, s.days, s.ranges[0]), (1, 0b011_1110, r(1020, 540)));
    let s = session_parse(b"0800-0900,1230-1630").unwrap();
    assert_eq!(
        (s.n, s.ranges[0], s.ranges[1]),
        (2, r(480, 540), r(750, 990))
    );
    let s = session_parse(b"24x7").unwrap();
    assert_eq!((s.n, s.days, s.ranges[0]), (1, 0x7F, r(0, 1440)));
    for bad in [
        "",
        "0930",
        "0930-",
        "2400-0100",
        "0960-1000",
        "0930-1600:",
        "0930-1600:8",
        "0930-1600:0",
        "930-1600",
        "0930-1600,",
        "0930_1600",
        "a930-1600",
        "0000-0100,0100-0200,0200-0300,0300-0400,0400-0500",
    ] {
        let e = session_parse(bad.as_bytes()).unwrap_err();
        assert_eq!(e.code, code::BAD_SESSION, "{bad}");
    }
}

fn off(name: &str, t_s: i64) -> i32 {
    tz_offset_s(tz_lookup(name.as_bytes()).unwrap(), t_s * 1000)
}

#[test]
fn tz_offsets_match_zoneinfo() {
    assert_eq!(tz_lookup(b"UTC"), Some(TZ_UTC));
    for z in ["UTC", "Etc/UTC", "GMT", "Etc/GMT"] {
        assert_eq!(off(z, 1_700_000_000), 0, "{z}");
    }
    // America/New_York: 2024-03-10 07:00:00Z springs forward, 2024-11-03 06:00:00Z falls back.
    assert_eq!(off("America/New_York", 1_710_053_999), -18_000);
    assert_eq!(off("America/New_York", 1_710_054_000), -14_400);
    assert_eq!(off("America/New_York", 1_730_613_599), -14_400);
    assert_eq!(off("America/New_York", 1_730_613_600), -18_000);
    // Europe/London 2039-03-27 01:00:00Z (footer-extended, past the TZif explicit table).
    assert_eq!(off("Europe/London", 2_184_800_399), 0);
    assert_eq!(off("Europe/London", 2_184_800_400), 3600);
    // Australia/Sydney: 2024-04-07 03:00 AEDT = 2024-04-06 16:00:00Z falls back to +10.
    assert_eq!(off("Australia/Sydney", 1_712_419_199), 39_600);
    assert_eq!(off("Australia/Sydney", 1_712_419_200), 36_000);
    // Fixed zones and history: Kolkata +5:30 throughout; Moscow +4 from 2011-03-27 to 2014-10-26.
    assert_eq!(off("Asia/Kolkata", 1_000_000_000), 19_800);
    assert_eq!(off("Asia/Calcutta", 2_000_000_000), 19_800);
    assert_eq!(off("Europe/Moscow", 1_350_000_000), 14_400);
    assert_eq!(off("Europe/Moscow", 1_500_000_000), 10_800);
    // Fixed offsets.
    assert_eq!(off("UTC+3", 0), 10_800);
    assert_eq!(off("UTC-5", 0), -18_000);
    assert_eq!(off("UTC+05:30", 0), 19_800);
    assert_eq!(off("GMT-0330", 0), -12_600);
    assert_eq!(off("UTC+14", 0), 50_400);
    for bad in [
        "",
        "utc",
        "UTC+15",
        "UTC+5:60",
        "UTC5",
        "Mars/Olympus",
        "America/new_york",
        "GMT+",
    ] {
        assert_eq!(tz_lookup(bad.as_bytes()), None, "{bad}");
    }
}

#[test]
fn tz_transitions_alternate_and_are_ordered() {
    // Both sides of every hour 2000–2040 in every DST zone: the offset changes only by whole
    // quarter-hours and stays within ±14 h.
    for z in [
        "America/New_York",
        "Europe/Berlin",
        "Pacific/Auckland",
        "America/Sao_Paulo",
    ] {
        let id = tz_lookup(z.as_bytes()).unwrap();
        let mut t = 946_684_800i64;
        let mut changes = 0;
        let mut prev = tz_offset_s(id, t * 1000);
        while t < 2_208_988_800 {
            let o = tz_offset_s(id, t * 1000);
            assert!(o % 900 == 0 && o.abs() <= 50_400, "{z} {t}");
            if o != prev {
                changes += 1;
            }
            prev = o;
            t += 3600;
        }
        assert!(changes > 0, "{z}");
    }
}

fn regex_find(p: &str, s: &str) -> Option<(usize, usize)> {
    regex_compile(p.as_bytes()).unwrap().find(s.as_bytes())
}

/// A reused slot (`regex_compile_into`, the run-time `RegexCache` path) answers exactly as a
/// fresh `regex_compile`, whatever the slot held before.
#[test]
fn regex_compile_into_reuses_a_slot() {
    let mut slot = RegexProg::EMPTY;
    let cases: [(&str, &str); 6] = [
        ("[^0-9]+(ab)*\\d{2,5}", "xyabab123456"),
        ("a", "bab"),
        ("^ab$", "ab"),
        (".", "\u{e9}x"),
        ("(a|)b", "zb"),
        ("x{3}", "xx"),
    ];
    for (p, s) in cases {
        regex_compile_into(p.as_bytes(), &mut slot).unwrap();
        assert_eq!(slot.find(s.as_bytes()), regex_find(p, s), "{p} on {s}");
    }
    assert!(regex_compile_into(b"(a", &mut slot).is_err());
}

#[test]
fn regex_subset_accepts_and_refuses() {
    for p in [
        "abc",
        "a.c",
        "\\d+",
        "\\D\\w\\W\\s\\S",
        "[a-z]+",
        "[^0-9]",
        "^ab$",
        "(ab)+",
        "a|b|c",
        "a*b+c?",
        "a{3}",
        "a{2,}",
        "a{1,32}",
        "[\\d.]+",
        "\\.\\(\\)",
        "",
        "(a|)",
        "[]a]",
        "[a-]",
    ] {
        assert!(regex_compile(p.as_bytes()).is_ok(), "{p}");
    }
    let long = "a".repeat(300);
    for p in [
        "a*?",
        "a+?",
        "a??",
        "a*+",
        "(?:a)",
        "(?=a)",
        "\\1",
        "\\b",
        "a{33}",
        "a{3,2}",
        "(a",
        "a)",
        "[a",
        "*a",
        "\\",
        "[z-a]",
        long.as_str(),
        "(((((((((((((((((a)))))))))))))))))",
    ] {
        let e = regex_compile(p.as_bytes())
            .err()
            .unwrap_or_else(|| panic!("{p} accepted"));
        assert_eq!(e.code, code::REGEX_UNSUPPORTED, "{p}");
    }
}

#[test]
fn regex_matches_reference_table() {
    let t: [(&str, &str, Option<(usize, usize)>); 22] = [
        ("abc", "xxabcxx", Some((2, 5))),
        ("a+", "baaab", Some((1, 4))),
        ("a*", "baaab", Some((0, 0))),
        ("\\d+", "ab12345cd", Some((2, 7))),
        ("a|ab", "ab", Some((0, 1))),
        ("ab|a", "ab", Some((0, 2))),
        ("^b", "ab", None),
        ("b$", "ab", Some((1, 2))),
        ("$", "ab", Some((2, 2))),
        ("[^a]+", "aabca", Some((2, 4))),
        ("a{2,3}", "aaaa", Some((0, 3))),
        ("a{2}", "a", None),
        ("(ab)+c", "ababc", Some((0, 5))),
        ("a.c", "a\nc abc", Some((4, 7))),
        ("\\s\\w", "x  y", Some((2, 4))),
        ("[\\d,]+", "v=1,234;", Some((2, 7))),
        ("x?y", "xxy", Some((1, 3))),
        ("", "abc", Some((0, 0))),
        ("a.c", "a\u{e9}c", Some((0, 4))),
        ("^.$", "\u{20ac}", Some((0, 3))),
        ("^..$", "\u{1f600}x", Some((0, 5))),
        ("a.b", "a\nb", None),
    ];
    for (p, s, want) in t {
        assert_eq!(regex_find(p, s), want, "{p} on {s:?}");
    }
}

/// `pin:04-regex-negclass`: `[^…]`, `\D \W \S` step one UTF-8 code point, like `.`; a negated
/// class listing a non-ASCII byte is refused.
#[test]
fn regex_negated_classes_are_code_point_wide() {
    let t: [(&str, &str, Option<(usize, usize)>); 10] = [
        ("[^a]", "\u{e9}", Some((0, 2))),
        ("^[^a]$", "\u{20ac}", Some((0, 3))),
        ("^[^0-9]{2}$", "\u{1f600}\u{e9}", Some((0, 6))),
        ("\\D+", "1\u{e9}2", Some((1, 3))),
        ("\\W", "a\u{e9}", Some((1, 3))),
        ("\\S\\S", "\u{1f600}x", Some((0, 5))),
        ("^\\D$", "\u{1f600}", Some((0, 4))),
        ("[^a]", "\n", Some((0, 1))),
        ("[^a]+", "aabca", Some((2, 4))),
        ("\\d\\D\\d", "1\u{20ac}2", Some((0, 5))),
    ];
    for (p, s, want) in t {
        assert_eq!(regex_find(p, s), want, "{p} on {s:?}");
    }
    let e = regex_compile("[^\u{e9}]".as_bytes())
        .err()
        .expect("refused");
    assert_eq!(e.code, code::REGEX_UNSUPPORTED);
}

proptest::proptest! {
    #[test]
    fn regex_never_panics_and_is_bounded(p in "[ab.|()*+?^$\\[\\]{}0-9,\\\\dws-]{0,24}", s in "[ab0-9 \n]{0,64}") {
        if let Ok(r) = regex_compile(p.as_bytes()) {
            if let Some((a, b)) = r.find(s.as_bytes()) {
                proptest::prop_assert!(a <= b && b <= s.len());
            }
        }
    }
}

#[test]
fn fmt_templates_accept_and_refuse() {
    for t in [
        "",
        "plain",
        "{0}",
        "x={0} y={1}",
        "{0,number}",
        "{0,number,#.##}",
        "{0,number,integer}",
        "{1,number,currency}",
        "{0,number,percent}",
        "{0,number,'$'#,##0.00}",
        "{0,date}",
        "{0,date,yyyy-MM-dd}",
        "{0,time,HH:mm:ss}",
        "it''s {0}",
        "'{0}' is literal",
        "{14}",
        "{0,number,0.0000000000000000}",
    ] {
        assert!(fmt_compile(t.as_bytes()).is_ok(), "{t}");
    }
    let many = "{0}x".repeat(17);
    for t in [
        "{",
        "}",
        "{0",
        "{0,}",
        "{15}",
        "{a}",
        "{0,string}",
        "{0,date,yyyy-QQ}",
        "{0,number,#;-#}",
        "{0,number,0.00000000000000000}",
        "{0,number,0#}",
        "{0,number,0.#0}",
        "{0,number,{}}",
        many.as_str(),
    ] {
        let e = fmt_compile(t.as_bytes())
            .err()
            .unwrap_or_else(|| panic!("{t} accepted"));
        assert_eq!(e.code, code::BAD_FORMAT, "{t}");
    }
    let p = fmt_compile(b"a{2}b{0,number,#.##}c").unwrap();
    assert_eq!((p.segments(), p.max_arg()), (5, Some(2)));
    assert_eq!(fmt_compile(b"it''s").unwrap().segments(), 2);
}

fn desc(kind: InputKind, ty: Ty, flags: u16, min: u64, max: u64) -> InputDesc {
    InputDesc {
        kind,
        ty,
        flags,
        min_bits: min,
        max_bits: max,
        ..InputDesc::default()
    }
}

#[test]
fn resolve_input_table() {
    use InputResolved::{Bits, Default as Dflt, Str};
    let mm = input_flag::HAS_MIN | input_flag::HAS_MAX;
    let i = desc(InputKind::Int, Ty::Int, mm, 1, 100);
    assert_eq!(resolve_input(&i, &[], None), Ok(Dflt));
    assert_eq!(
        resolve_input(&i, &[], Some(&InputValue::Int(14))),
        Ok(Bits(14))
    );
    let range = |r: Result<InputResolved<'_>, LoadErr>| r.unwrap_err().code == code::INPUT_RANGE;
    let ty = |r: Result<InputResolved<'_>, LoadErr>| r.unwrap_err().code == code::INPUT_TYPE;
    assert!(range(resolve_input(&i, &[], Some(&InputValue::Int(101)))));
    assert!(range(resolve_input(&i, &[], Some(&InputValue::Int(0)))));
    assert!(ty(resolve_input(
        &i,
        &[],
        Some(&InputValue::Float1e6(14_000_000))
    )));
    let opts = [InputValue::Int(7), InputValue::Int(14)];
    assert!(range(resolve_input(&i, &opts, Some(&InputValue::Int(8)))));
    assert_eq!(
        resolve_input(&i, &opts, Some(&InputValue::Int(7))),
        Ok(Bits(7))
    );

    let f = desc(
        InputKind::Float,
        Ty::Float,
        mm,
        0.5f64.to_bits(),
        2.0f64.to_bits(),
    );
    assert_eq!(
        resolve_input(&f, &[], Some(&InputValue::Float1e6(1_500_000))),
        Ok(Bits(1.5f64.to_bits()))
    );
    assert!(range(resolve_input(
        &f,
        &[],
        Some(&InputValue::Float1e6(2_500_000))
    )));
    assert!(
        ty(resolve_input(&f, &[], Some(&InputValue::Int(1)))),
        "N3: an Int for a float is 5007"
    );
    let p = desc(InputKind::Price, Ty::Float, 0, 0, 0);
    assert!(ty(resolve_input(&p, &[], Some(&InputValue::Int(1)))));

    let b = desc(InputKind::Bool, Ty::Bool, 0, 0, 0);
    assert_eq!(
        resolve_input(&b, &[], Some(&InputValue::Bool(true))),
        Ok(Bits(1))
    );
    assert!(ty(resolve_input(&b, &[], Some(&InputValue::Int(1)))));

    let c = desc(InputKind::Color, Ty::Color, 0, 0, 0);
    let red = color::pack(255, 0, 0, 30);
    assert_eq!(
        resolve_input(&c, &[], Some(&InputValue::Int(red))),
        Ok(Bits(red as u64))
    );
    assert!(range(resolve_input(
        &c,
        &[],
        Some(&InputValue::Int(101 << 24))
    )));
    assert!(range(resolve_input(&c, &[], Some(&InputValue::Int(-1)))));

    for k in [InputKind::Time, InputKind::Enum] {
        let d = desc(k, Ty::Int, 0, 0, 0);
        assert_eq!(
            resolve_input(&d, &[], Some(&InputValue::Int(3))),
            Ok(Bits(3))
        );
        assert!(ty(resolve_input(&d, &[], Some(&InputValue::Bool(true)))));
    }

    let tf = desc(InputKind::Timeframe, Ty::String, 0, 0, 0);
    let ok = InputValue::Str("240".into());
    assert_eq!(resolve_input(&tf, &[], Some(&ok)), Ok(Str(b"240")));
    let e = resolve_input(&tf, &[], Some(&InputValue::Str("1H".into()))).unwrap_err();
    assert_eq!(e.code, code::BAD_TIMEFRAME);
    let ss = desc(InputKind::Session, Ty::String, 0, 0, 0);
    let e = resolve_input(&ss, &[], Some(&InputValue::Str("25:00".into()))).unwrap_err();
    assert_eq!(e.code, code::BAD_SESSION);
    for k in [
        InputKind::String,
        InputKind::TextArea,
        InputKind::Symbol,
        InputKind::Source,
    ] {
        let d = desc(k, Ty::String, 0, 0, 0);
        let v = InputValue::Str("close".into());
        assert_eq!(resolve_input(&d, &[], Some(&v)), Ok(Str(b"close")));
        assert!(ty(resolve_input(&d, &[], Some(&InputValue::Int(1)))));
        let opts = [InputValue::Str("open".into())];
        assert!(range(resolve_input(&d, &opts, Some(&v))));
    }
}

#[test]
fn load_errors_use_std_codes_only() {
    let inputs: [&[u8]; 12] = [
        b"",
        b"(",
        b"{",
        b"}",
        b"1H",
        b"x",
        b"0930",
        b"[",
        b"\\",
        b"99999999999D",
        b"a{99}",
        b"\xff",
    ];
    let mut errs: Vec<LoadErr> = Vec::new();
    for s in inputs {
        errs.extend(tf_seconds(s).err());
        errs.extend(session_parse(s).err());
        errs.extend(regex_compile(s).err());
        errs.extend(fmt_compile(s).err());
    }
    errs.extend(tf_on_bar_clock(7).err());
    assert!(!errs.is_empty());
    for e in errs {
        assert!((5001..=5007).contains(&e.code.0), "{e:?}");
        assert!(!e.why.is_empty());
    }
}
