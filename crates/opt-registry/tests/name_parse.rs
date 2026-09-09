// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Deribit option-name parser tests.
//!
//! The table cases are REAL instrument names taken from live capture run
//! dirs' `instrument-manifest.tsv` on 2026-09-09, and from the pinned
//! live-wire fixture in `ingress-deribit/src/lib.rs:1945`. The expiry
//! values are independently derived (08:00 UTC on the named date) — one
//! of them, `BTC-8SEP26`, also appears as `expiry_ms 1788854400000` in
//! the captured option history, which cross-checks the hour law against
//! venue data rather than against this parser.

use opt_registry::{
    parse_deribit_descriptor, parse_deribit_option_name, strip_deribit_prefix, RIGHT_CALL,
    RIGHT_PUT,
};

const SEC: u64 = 1_000_000_000;

#[test]
fn real_names_parse_exactly() {
    // (name, ccy, expiry_seconds, strike_1e6, right)
    let cases: &[(&[u8], &[u8], u64, i64, u8)] = &[
        (
            b"BTC-10SEP26-79000-C",
            b"BTC",
            1_789_027_200,
            79_000_000_000,
            RIGHT_CALL,
        ),
        (
            b"BTC-10SEP26-78000-P",
            b"BTC",
            1_789_027_200,
            78_000_000_000,
            RIGHT_PUT,
        ),
        // one-digit day: the other length branch
        (
            b"BTC-8SEP26-79000-C",
            b"BTC",
            1_788_854_400,
            79_000_000_000,
            RIGHT_CALL,
        ),
        (
            b"ETH-11SEP26-2560-P",
            b"ETH",
            1_789_113_600,
            2_560_000_000,
            RIGHT_PUT,
        ),
        // the pinned live-wire fixture's instrument
        (
            b"BTC-27MAR26-100000-C",
            b"BTC",
            1_774_598_400,
            100_000_000_000,
            RIGHT_CALL,
        ),
        (
            b"BTC-10JUN26-63000-C",
            b"BTC",
            1_781_078_400,
            63_000_000_000,
            RIGHT_CALL,
        ),
        // leap day, and the far end of the 2-digit year window
        (
            b"BTC-29FEB28-50000-C",
            b"BTC",
            1_835_424_000,
            50_000_000_000,
            RIGHT_CALL,
        ),
        (
            b"BTC-1JAN27-50000-P",
            b"BTC",
            1_798_790_400,
            50_000_000_000,
            RIGHT_PUT,
        ),
        (
            b"BTC-31DEC99-1-C",
            b"BTC",
            4_102_387_200,
            1_000_000,
            RIGHT_CALL,
        ),
    ];
    for (name, ccy, secs, strike, right) in cases {
        let p = parse_deribit_option_name(name)
            .unwrap_or_else(|| panic!("failed to parse {:?}", core::str::from_utf8(name)));
        assert_eq!(p.ccy, *ccy, "ccy of {:?}", core::str::from_utf8(name));
        assert_eq!(
            p.expiry_ns,
            secs * SEC,
            "expiry of {:?}",
            core::str::from_utf8(name)
        );
        assert_eq!(
            p.strike_1e6, *strike,
            "strike of {:?}",
            core::str::from_utf8(name)
        );
        assert_eq!(
            p.right, *right,
            "right of {:?}",
            core::str::from_utf8(name)
        );
    }
}

#[test]
fn descriptor_form_strips_the_venue_prefix() {
    let bare = parse_deribit_option_name(b"BTC-10SEP26-79000-C").expect("bare");
    let desc = parse_deribit_descriptor(b"deribit:BTC-10SEP26-79000-C").expect("descriptor");
    assert_eq!(bare, desc);
    // The bare form must also survive the descriptor entry point.
    assert_eq!(
        parse_deribit_descriptor(b"BTC-10SEP26-79000-C").expect("bare via descriptor"),
        bare
    );
    assert_eq!(strip_deribit_prefix(b"deribit:X"), b"X");
    assert_eq!(strip_deribit_prefix(b"X"), b"X");
    // A bare prefix with nothing after it is not stripped into empty.
    assert_eq!(strip_deribit_prefix(b"deribit:"), b"deribit:");
}

/// THE important negative test: the two sibling grammars that ship in the
/// same manifest must never parse here. Both would otherwise mint a
/// wrong expiry rather than fail.
#[test]
fn sibling_venue_grammars_are_refused() {
    // OKX: five dash-fields.
    assert!(parse_deribit_option_name(b"BTC-USD-260910-79000-C").is_none());
    assert!(parse_deribit_descriptor(b"okx:BTC-USD-260910-79000-C").is_none());
    // Binance eapi: four fields, but the date is YYMMDD with no month name.
    assert!(parse_deribit_option_name(b"BTC-260910-79000-C").is_none());
    assert!(parse_deribit_descriptor(b"binance-opt:BTC-260910-79000-C").is_none());
}

#[test]
fn non_option_deribit_descriptors_are_refused() {
    // Statics from the same manifest — these share the file, not the grammar.
    assert!(parse_deribit_descriptor(b"deribit:BTC-PERPETUAL").is_none());
    assert!(parse_deribit_descriptor(b"deribit:BTC_USDC").is_none());
    assert!(parse_deribit_descriptor(b"deribit:BTC_USDC-PERPETUAL").is_none());
    assert!(parse_deribit_descriptor(b"deribit:ETH_USDC-PERPETUAL").is_none());
}

#[test]
fn malformed_names_are_refused() {
    let bad: &[&[u8]] = &[
        b"",
        b"-",
        b"---",
        b"BTC-10SEP26-79000",          // three fields
        b"BTC-10SEP26-79000-C-X",      // five fields
        b"BTC-10SEP26-79000-X",        // right is neither C nor P
        b"BTC-10SEP26-79000-c",        // lowercase right
        b"-10SEP26-79000-C",           // empty currency
        b"BTC-10sep26-79000-C",        // lowercase month
        b"BTC-10XXX26-79000-C",        // not a month
        b"BTC-260910-79000-C",         // no month name
        b"BTC-32SEP26-79000-C",        // day out of range
        b"BTC-0SEP26-79000-C",         // day zero
        b"BTC-31SEP26-79000-C",        // September has 30 days
        b"BTC-29FEB27-79000-C",        // 2027 is not a leap year
        b"BTC-10SEP2-79000-C",         // year too short
        b"BTC-10SEP266-79000-C",       // year too long
        b"BTC-1O SEP26-79000-C",       // letter O, and a space
        b"BTC-10SEP26--C",             // empty strike
        b"BTC-10SEP26-0-C",            // zero strike
        b"BTC-10SEP26-79000.-C",       // trailing dot
        b"BTC-10SEP26-.5-C",           // leading dot
        b"BTC-10SEP26-1.1234567-C",    // seven fractional digits
        b"BTC-10SEP26-79 000-C",       // space inside the strike
        b"BTC-10SEP26-79000d5-C",      // Deribit's d-decimal spelling: refused
        b"BTC-10SEP26-9999999999999-C", // strike overflows the x1e6 field
    ];
    for b in bad {
        assert!(
            parse_deribit_option_name(b).is_none(),
            "should have refused {:?}",
            core::str::from_utf8(b)
        );
    }
}

#[test]
fn fractional_strikes_scale_exactly() {
    let p = parse_deribit_option_name(b"XRP-10SEP26-2.5-C").expect("fractional strike");
    assert_eq!(p.strike_1e6, 2_500_000);
    let p = parse_deribit_option_name(b"XRP-10SEP26-0.000001-P").expect("one micro-unit");
    assert_eq!(p.strike_1e6, 1);
    // Six digits is the limit and must still be exact.
    let p = parse_deribit_option_name(b"XRP-10SEP26-1.234567-C").expect("six digits");
    assert_eq!(p.strike_1e6, 1_234_567);
}

proptest::proptest! {
    /// A parser that touches operator data must never panic. Arbitrary
    /// bytes, arbitrary length — the only acceptable outcomes are a
    /// parse and a `None`.
    #[test]
    fn never_panics_on_arbitrary_bytes(raw in proptest::collection::vec(proptest::num::u8::ANY, 0..64)) {
        let _ = parse_deribit_option_name(&raw);
        let _ = parse_deribit_descriptor(&raw);
        let _ = strip_deribit_prefix(&raw);
    }

    /// The same, biased to the shape of a real name so the generator
    /// actually reaches the numeric branches rather than bouncing off
    /// the field-count test.
    #[test]
    fn never_panics_on_name_shaped_input(s in "[A-Za-z0-9.]{0,6}-[A-Za-z0-9]{0,8}-[0-9.]{0,12}-[A-Za-z]{0,2}") {
        let _ = parse_deribit_option_name(s.as_bytes());
    }

    /// Any name we DO accept round-trips its own fields sanely: a
    /// positive strike, an expiry on an exact 08:00 UTC boundary, and a
    /// right that is one of the two legal bytes.
    #[test]
    fn accepted_names_are_self_consistent(
        day in 1u32..=28,
        mon in proptest::sample::select(vec!["JAN","FEB","MAR","APR","MAY","JUN","JUL","AUG","SEP","OCT","NOV","DEC"]),
        yy in 0u32..=99,
        strike in 1u64..=1_000_000,
        call in proptest::bool::ANY,
    ) {
        let name = format!("BTC-{}{}{:02}-{}-{}", day, mon, yy, strike, if call { "C" } else { "P" });
        let p = parse_deribit_option_name(name.as_bytes()).expect("well-formed");
        proptest::prop_assert_eq!(p.ccy, b"BTC");
        proptest::prop_assert!(p.strike_1e6 > 0);
        proptest::prop_assert_eq!(p.strike_1e6, strike as i64 * 1_000_000);
        proptest::prop_assert_eq!(p.right, if call { RIGHT_CALL } else { RIGHT_PUT });
        // 08:00 UTC exactly: seconds since midnight must be 8 h.
        let secs = p.expiry_ns / SEC;
        proptest::prop_assert_eq!(secs % 86_400, 8 * 3_600);
    }
}
