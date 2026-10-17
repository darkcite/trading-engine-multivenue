// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.4 — the one tolerance. This edition carries the `dec` rows (T2); `cmp_*` and
//! `trades_*` land with T16.

use pine_conformance::dec::{money_ok, parse_dec, scaled_half_away, Dec, DecErr};

fn d(s: &str) -> Dec {
    parse_dec(s.as_bytes()).unwrap()
}

#[test]
fn dec_scaled_half_away_rounds_away_from_zero() {
    assert_eq!(scaled_half_away(d("60123.4567895"), 6), 60_123_456_790);
    assert_eq!(scaled_half_away(d("-12.3456785"), 6), -12_345_679);
    assert_eq!(scaled_half_away(d("0.0000005"), 6), 1);
    assert_eq!(scaled_half_away(d("-0.0000005"), 6), -1);
    assert_eq!(scaled_half_away(d("0.0000004999"), 6), 0);
    assert_eq!(scaled_half_away(d("60123.5"), 6), 60_123_500_000);
    assert_eq!(scaled_half_away(d("1"), 6), 1_000_000);
    assert_eq!(scaled_half_away(d("-0.00"), 6), 0);
}

#[test]
fn dec_parse_refuses_separators_and_exponents() {
    assert_eq!(parse_dec(b"1,000.5"), Err(DecErr::BadChar(b',')));
    assert_eq!(parse_dec(b"1e5"), Err(DecErr::BadChar(b'e')));
    assert_eq!(parse_dec(b"1 000"), Err(DecErr::BadChar(b' ')));
    assert_eq!(parse_dec(b"1.2.3"), Err(DecErr::BadChar(b'.')));
    assert_eq!(parse_dec(b".5"), Err(DecErr::BadChar(b'.')));
    assert_eq!(parse_dec(b"5."), Err(DecErr::BadChar(b'.')));
    assert_eq!(parse_dec(b""), Err(DecErr::Empty));
    assert_eq!(parse_dec(b"-"), Err(DecErr::Empty));
    assert_eq!(parse_dec(&[b'9'; 37]), Err(DecErr::TooLong));
    assert!(parse_dec(&[b'9'; 36]).is_ok());
    assert_eq!(
        d("-376.50"),
        Dec {
            neg: true,
            m: 37650,
            d: 2
        }
    );
    assert_eq!(
        d("+1.5"),
        Dec {
            neg: false,
            m: 15,
            d: 1
        }
    );
    for t in ["376.50", "-0.00", "0.0000005", "12", "-12.3456785"] {
        assert_eq!(d(t).render(), t);
    }
}

#[test]
fn dec_money_is_within_half_a_printed_unit() {
    let p = d("376.50");
    let lo: i128 = 376_495_000_000_000;
    let hi: i128 = 376_505_000_000_000;
    assert!(money_ok(lo, p));
    assert!(money_ok(hi, p));
    assert!(money_ok(376_500_000_000_000, p));
    assert!(!money_ok(lo - 1, p));
    assert!(!money_ok(hi + 1, p));
    // Negative values and a printed precision finer than ×1e12.
    assert!(money_ok(-376_505_000_000_000, d("-376.50")));
    assert!(!money_ok(-376_505_000_000_001, d("-376.50")));
    // 13 decimals: half a printed unit is 0.05 ×1e-12, so only the exact unit matches.
    let fine = d("0.0000000000010");
    assert!(money_ok(1, fine) && !money_ok(0, fine) && !money_ok(2, fine));
    assert!(!money_ok(1, d("0.0000000000005")) && !money_ok(0, d("0.0000000000005")));
    // Overflow fails closed.
    assert!(!money_ok(i128::MAX, d("1")));
}
