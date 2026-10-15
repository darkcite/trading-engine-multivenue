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
fn fold_floored_mod_matches_the_formula() {
    let rd = v5();
    assert_eq!(fold::f_mod(-1.0, 100.0, &rd), 99.0);
    assert_eq!(fold::f_mod(1.0, -100.0, &rd), -99.0);
    assert_eq!(fold::f_mod(7.5, 2.0, &rd), 1.5);
    assert!(fold::f_mod(1.0, 0.0, &rd).is_nan());
    assert_eq!(fold::i_mod(-1, 100), Some(99));
    assert_eq!(fold::i_mod(1, -100), Some(-99));
    assert_eq!(fold::i_mod(i64::MIN, -1), Some(0));
    assert_eq!(fold::i_mod(5, 0), None);
}

#[test]
fn fold_int_arith_wraps() {
    assert_eq!(fold::i_add(i64::MAX, 1), i64::MIN);
    assert_eq!(fold::i_sub(i64::MIN, 1), i64::MAX);
    assert_eq!(fold::i_mul(i64::MAX, 2), -2);
}

#[test]
fn fold_int_div_reads_w1() {
    let v5 = v5();
    assert_eq!(fold::i_div(7, 2, &v5), IntDiv::Int(Some(3)));
    assert_eq!(fold::i_div(-7, 2, &v5), IntDiv::Int(Some(-3)));
    assert_eq!(fold::i_div(1, 0, &v5), IntDiv::Int(None));
    assert_eq!(fold::i_div(i64::MIN, -1, &v5), IntDiv::Int(None));
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
    let mut rd = v5();
    assert_eq!(rd.round_ties, pine_spec::swr::ROUND_TIES_HALF_UP);
    assert_eq!(fold::round(-2.5, &rd), Some(-2));
    assert_eq!(fold::round(2.5, &rd), Some(3));
    rd.round_ties = pine_spec::swr::ROUND_TIES_HALF_AWAY;
    assert_eq!(fold::round(-2.5, &rd), Some(-3));
    assert_eq!(fold::round(f64::NAN, &rd), None);
}

#[test]
fn fold_int_cast_reads_the_pin() {
    let mut rd = v5();
    assert_eq!(fold::int_of_float(2.7, &rd), Some(2));
    assert_eq!(fold::int_of_float(-2.7, &rd), Some(-2));
    assert_eq!(fold::int_of_float(f64::INFINITY, &rd), None);
    assert_eq!(fold::int_of_float(9.3e18, &rd), None);
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
