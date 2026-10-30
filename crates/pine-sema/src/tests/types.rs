// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! §4.4.2 qualifiers and §4.4.3 conversions.

use pine_ir::{elem, Dialect, Qual, Ty, TypeQ};

use crate::sw::Readings;
use crate::types::{admits, conv, join, Conv};

fn s(ty: Ty) -> TypeQ {
    TypeQ::scalar(ty, Qual::Series)
}

#[test]
fn types_qualifier_join_and_admission() {
    assert_eq!(join(Qual::Const, Qual::Series), Qual::Series);
    assert_eq!(join(Qual::Simple, Qual::Input), Qual::Simple);
    assert_eq!(join(Qual::Const, Qual::Const), Qual::Const);
    assert!(admits(Qual::Series, Qual::Const));
    assert!(admits(Qual::Simple, Qual::Simple));
    assert!(!admits(Qual::Simple, Qual::Series));
    assert!(!admits(Qual::Const, Qual::Input));
}

#[test]
fn types_conversions_per_dialect() {
    let (v5, v6) = (Readings::of(Dialect::V5), Readings::of(Dialect::V6));
    assert_eq!(
        conv(s(Ty::Int), s(Ty::Float), Dialect::V5, &v5),
        Some(Conv::IntToFloat)
    );
    assert_eq!(conv(s(Ty::Float), s(Ty::Int), Dialect::V5, &v5), None);
    assert_eq!(
        conv(s(Ty::Float), s(Ty::Bool), Dialect::V5, &v5),
        Some(Conv::NumToBool)
    );
    assert_eq!(
        conv(s(Ty::Float), s(Ty::Bool), Dialect::V6, &v6),
        None,
        "MG-4"
    );
    assert_eq!(
        conv(s(Ty::Void), s(Ty::Bool), Dialect::V5, &v5),
        Some(Conv::Na)
    );
    assert_eq!(
        conv(s(Ty::Void), s(Ty::Bool), Dialect::V6, &v6),
        None,
        "W13"
    );
    assert_eq!(
        conv(s(Ty::Void), s(Ty::Float), Dialect::V6, &v6),
        Some(Conv::Na)
    );
    assert_eq!(conv(s(Ty::String), s(Ty::Color), Dialect::V5, &v5), None);
    let af = TypeQ::array(elem::scalar(Ty::Float), Qual::Series);
    let ai = TypeQ::array(elem::scalar(Ty::Int), Qual::Series);
    assert_eq!(conv(af, af, Dialect::V6, &v6), Some(Conv::Same));
    assert_eq!(
        conv(ai, af, Dialect::V6, &v6),
        None,
        "collections convert only when identical"
    );
    let u0 = TypeQ { a: 0, ..s(Ty::Udt) };
    let u1 = TypeQ { a: 1, ..s(Ty::Udt) };
    assert_eq!(
        conv(u0, u1, Dialect::V5, &v5),
        None,
        "distinct declarations"
    );
}
