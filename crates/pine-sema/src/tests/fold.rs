// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! The §4.17 fold table, every reading (I16, I22's formulas).

use pine_ir::Dialect;

use crate::fold::{self, IntDiv};
use crate::pins::{ColorAlpha, IntCast};
use crate::sw::Readings;

fn v5() -> Readings {
    Readings::of(Dialect::V5)
}

#[test]
fn fold_truncated_mod_has_the_dividends_sign() {
    // CAR-R31-1 (TradingView F004 F039 F115 F116): `807 % -7 == 2`, `-807 % 7 == -2`,
    // `807.25 % -2.5 == 2.25`.
    let rd = v5();
    assert_eq!(fold::f_mod(-1.0, 100.0, &rd), -1.0);
    assert_eq!(fold::f_mod(1.0, -100.0, &rd), 1.0);
    assert_eq!(fold::f_mod(7.5, 2.0, &rd), 1.5);
    assert_eq!(fold::f_mod(807.25, -2.5, &rd), 2.25);
    assert_eq!(fold::f_mod(-807.25, 2.5, &rd), -2.25);
    assert!(fold::f_mod(1.0, 0.0, &rd).is_nan());
    assert_eq!(fold::i_mod(-1, 100), Some(-1));
    assert_eq!(fold::i_mod(1, -100), Some(1));
    assert_eq!(fold::i_mod(807, -7), Some(2));
    assert_eq!(fold::i_mod(-807, 7), Some(-2));
    assert_eq!(fold::i_mod(i64::MIN, -1), Some(0));
    assert_eq!(fold::i_mod(5, 0), None);
}

#[test]
fn fold_float_rounds_to_16_decimal_places() {
    // CAR-R33-4 as amended by r39 (TradingView F098 folded columns, F124): a folded float result is
    // the VM's result rounded to 16 decimal places; the series columns keep the exact remainder
    // (`f_mod` itself).
    let rd = v5();
    assert_eq!(fold::f_mod(1.0, 0.1, &rd), 0.09999999999999995);
    assert_eq!(fold::dec16(fold::f_mod(1.0, 0.1, &rd)), 0.1);
    assert_eq!(fold::dec16(fold::f_mod(10.0, 0.1, &rd)), 0.0999999999999995);
    assert_eq!(fold::dec16(fold::f_mod(-0.3, 0.1, &rd)), -0.1);
    assert_eq!(fold::dec16(fold::f_mod(807.25, -2.5, &rd)), 2.25);
    // F124: `+ − *` fold to the 16-decimal grid; `/` already lies on it; 16 significant digits
    // above 1 are kept (the r33 15-digit rule would give 1234567890123.12).
    assert_eq!(fold::dec16(0.1 + 0.2), 0.3);
    assert_eq!(fold::dec16(0.3 - 0.1), 0.2);
    assert_eq!(fold::dec16(0.1 * 3.0), 0.3);
    assert_eq!(fold::dec16(1.0 / 3.0), 0.3333333333333333);
    assert_eq!(fold::dec16(2.0 / 3.0), 0.6666666666666666);
    assert_eq!(fold::dec16(1.0 + f64::EPSILON), 1.0 + f64::EPSILON);
    assert_eq!(
        fold::dec16(fold::f_mod(1_234_567_890_123.125, 1e15, &rd)),
        1_234_567_890_123.125
    );
    assert!(fold::dec16(fold::f_mod(1.0, 0.0, &rd)).is_nan());
    assert_eq!(fold::dec16(f64::INFINITY), f64::INFINITY);
    assert_eq!(fold::dec16(-0.0).to_bits(), (-0.0f64).to_bits());
    assert_eq!(fold::dec16(1e-17), 0.0);
}

/// CAR-R33-5 (TradingView F011 F037 F040 F047 F120): binary64, no wrap; `K::Int(i64::MAX)` stands
/// for `2^63`; beyond `±2^63` no fold (the VM's f64 lane holds it).
#[test]
fn fold_int_arith_is_binary64() {
    assert_eq!(fold::i_add(i64::MAX, 1), Some(i64::MAX), "2^63 + 1 is 2^63");
    assert_eq!(
        fold::i_sub(i64::MIN, 1),
        Some(i64::MIN),
        "-2^63 - 1 is -2^63"
    );
    assert_eq!(
        fold::i_mul(i64::MAX, 2),
        None,
        "2^64: beyond the representatives"
    );
    assert_eq!(
        fold::i_mul(3_037_000_500, 3_037_000_500),
        None,
        "F037: > 2^63"
    );
    assert_eq!(
        fold::i_add(9_007_199_254_740_992, 1),
        Some(9_007_199_254_740_992)
    );
    assert_eq!(fold::i_neg(i64::MIN), Some(i64::MAX), "-(-2^63) is 2^63");
    assert_eq!(
        fold::int_rep(9_007_199_254_740_993.0),
        Some(9_007_199_254_740_992)
    );
    assert_eq!(fold::int_rep(0.5), None);
    assert_eq!(fold::int_rep(f64::NAN), None);
    assert_eq!(fold::i_mod(-7, 2), Some(-1));
    assert_eq!(fold::i_mod(i64::MIN, -1), Some(0));
    assert_eq!(fold::i_mod(1, 0), None);
}

#[test]
fn fold_int_div_reads_w1() {
    let v5 = v5();
    assert_eq!(fold::i_div(7, 2, &v5), IntDiv::Int(Some(3)));
    assert_eq!(fold::i_div(-7, 2, &v5), IntDiv::Int(Some(-3)));
    assert_eq!(fold::i_div(1, 0, &v5), IntDiv::Int(None));
    assert_eq!(
        fold::i_div(i64::MIN, -1, &v5),
        IntDiv::Int(Some(i64::MAX)),
        "2^63"
    );
    let v6 = Readings::of(Dialect::V6);
    assert_eq!(fold::i_div(7, 2, &v6), IntDiv::Float(3.5));
    let IntDiv::Float(q) = fold::i_div(1, 0, &v6) else {
        panic!("fraction")
    };
    assert!(q.is_nan(), "U15 na");
}

#[test]
fn fold_float_div_zero_reads_u15() {
    let mut rd = v5();
    assert!(fold::f_div(1.0, 0.0, &rd).is_nan());
    rd.float_div_zero = pine_spec::swr::FLOAT_DIV_ZERO_IEEE;
    assert_eq!(fold::f_div(1.0, 0.0, &rd), f64::INFINITY);
    assert_eq!(fold::f_div(-1.0, 0.0, &rd), f64::NEG_INFINITY);
}

#[test]
fn fold_round_reads_u04() {
    // CAR-R33-8 (F015, 64/64): ties go away from zero by default in both dialects.
    let mut rd = v5();
    assert_eq!(rd.round_ties, pine_spec::swr::ROUND_TIES_HALF_AWAY);
    assert_eq!(fold::round(-2.5, &rd), Some(-3));
    assert_eq!(fold::round(-0.5, &rd), Some(-1));
    assert_eq!(fold::round(2.5, &rd), Some(3));
    assert_eq!(fold::round(f64::NAN, &rd), None);
    rd.round_ties = pine_spec::swr::ROUND_TIES_HALF_UP;
    assert_eq!(fold::round(-2.5, &rd), Some(-2));
}

#[test]
fn fold_int_cast_reads_the_pin() {
    let mut rd = v5();
    assert_eq!(fold::int_of_float(2.7, &rd), Some(2));
    assert_eq!(fold::int_of_float(-2.7, &rd), Some(-2));
    assert_eq!(fold::int_of_float(f64::INFINITY, &rd), None);
    // CAR-R33-5 (TradingView F120): beyond ±2^63 clamps — `int(1e30)` is 2^63 (`i64::MAX`).
    assert_eq!(fold::int_of_float(9.3e18, &rd), Some(i64::MAX));
    assert_eq!(fold::int_of_float(1e30, &rd), Some(i64::MAX));
    assert_eq!(fold::int_of_float(-1e30, &rd), Some(i64::MIN));
    assert_eq!(
        fold::int_of_float(-9_223_372_036_854_775_808.0, &rd),
        Some(i64::MIN)
    );
    rd.pins.int_cast = IntCast::RoundHalfAway;
    assert_eq!(fold::int_of_float(2.7, &rd), Some(3));
    assert_eq!(fold::int_of_float(-2.5, &rd), Some(-3));
}

#[test]
fn fold_alpha_maps_to_transparency() {
    let mut rd = v5();
    assert_eq!(fold::alpha_to_transp(0x00, &rd), 100);
    assert_eq!(fold::alpha_to_transp(0xFF, &rd), 0);
    assert_eq!(fold::alpha_to_transp(0x80, &rd), 50);
    assert_eq!(
        fold::color_literal(255, 0, 0, None, &rd),
        pine_ir::color::pack(255, 0, 0, 0)
    );
    rd.pins.color_alpha = ColorAlpha::Truncate;
    assert_eq!(fold::alpha_to_transp(0x00, &rd), 100);
    assert_eq!(fold::alpha_to_transp(0xFF, &rd), 0);
    assert_eq!(fold::alpha_to_transp(0x80, &rd), 49);
}
