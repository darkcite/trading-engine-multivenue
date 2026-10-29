// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `inputs.rs` (I27).

mod common;

use common::ind;
use pine_corpus::inputs::{decoded, k_numeric, present};
use pine_corpus::scan::scan;
use pine_ir::{input_flag, InputKind, Ty};

#[test]
fn inputs_bounds_flags_match_decoded_literals() {
    let s = scan(&ind("lo = 3\na = input.int(14, \"A\", minval = 1, maxval = 50)\nb = input.int(14, \"B\", minval = lo)\nplot(close * a * b)"));
    let a = s.inputs[0];
    assert_eq!(
        a.desc.flags & (input_flag::HAS_MIN | input_flag::HAS_MAX),
        input_flag::HAS_MIN | input_flag::HAS_MAX
    );
    assert_eq!(a.desc.min_bits, 1);
    assert_eq!(a.desc.max_bits, 50);
    let b = s.inputs[1];
    assert_eq!(b.desc.flags & input_flag::HAS_MIN, 0);
    assert_ne!(b.present & present::MINVAL, 0);
}

#[test]
fn inputs_overridden_is_never_set() {
    let s = scan(&ind("a = input.int(1)\nb = input.float(2.0)\nc = input.bool(true)\nd = input.string(\"x\")\nplot(close)"));
    assert!(s
        .inputs
        .iter()
        .all(|i| i.desc.flags & input_flag::OVERRIDDEN == 0));
}

#[test]
fn inputs_dead_only_for_unread_bindings() {
    let s = scan(&ind("used = input.int(1, \"u\")\nunused = input.int(2, \"v\")\nplot(close * used * input.int(3, \"inline\"))"));
    assert_eq!(s.inputs.len(), 3);
    assert_eq!(s.inputs[0].desc.flags & input_flag::DEAD, 0);
    assert_ne!(s.inputs[1].desc.flags & input_flag::DEAD, 0);
    assert_eq!(s.inputs[2].desc.flags & input_flag::DEAD, 0);
    assert_eq!(s.inputs[2].decoded & decoded::NAME, 0);
    assert_eq!(s.syms.resolve(s.inputs[2].desc.name), "inline");
}

#[test]
fn inputs_value_encoding_follows_pine_ir() {
    let s = scan(&ind("a = input.float(1)\nb = input.string(\"a\")\nc = input.int(-5)\nd = input.bool(true)\nplot(close)"));
    assert_eq!(s.inputs[0].desc.default_bits, 1.0f64.to_bits());
    assert_eq!(s.inputs[0].desc.ty, Ty::Float);
    let sym = s.inputs[1].desc.default_bits as u32;
    assert_eq!(s.syms.resolve(pine_ir::Sym(sym)), "a");
    assert_eq!(s.inputs[2].desc.default_bits as i64, -5);
    assert_eq!(s.inputs[3].desc.default_bits, 1);
    assert!(s.inputs.iter().all(|i| i.decoded & decoded::DEFAULT != 0));
}

#[test]
fn inputs_generic_input_takes_its_kind_from_defval() {
    let s = scan(&ind(
        "a = input(14)\nb = input(close)\nc = input(1.5)\nplot(close * a * b * c)",
    ));
    assert_eq!(s.inputs[0].desc.kind, InputKind::Int);
    assert_eq!(s.inputs[1].desc.kind, InputKind::Source);
    assert_eq!(s.inputs[2].desc.kind, InputKind::Float);
    assert!(s.inputs.iter().all(|i| i.decoded & decoded::KIND != 0));
}

#[test]
fn inputs_ambiguous_positional_arguments_stay_unnamed() {
    // Position 2 is `minval` in one overload and `options` in the other; a non-literal binds
    // either, so it stays unnamed: neither MINVAL nor OPTIONS is present.
    let s = scan(&ind("v = 3\na = input.int(14, \"A\", v)\nplot(close * a)"));
    let i = s.inputs[0];
    assert_eq!(i.present & (present::MINVAL | present::OPTIONS), 0);
    assert_ne!(i.present & present::DEFVAL, 0);
    assert_ne!(i.present & present::TITLE, 0);
    // A literal disambiguates: a scalar never binds `options`.
    let s = scan(&ind("a = input.int(14, \"A\", 2)\nplot(close * a)"));
    assert_ne!(s.inputs[0].present & present::MINVAL, 0);
    assert_ne!(s.inputs[0].desc.flags & input_flag::HAS_MIN, 0);
}

#[test]
fn inputs_k_numeric_counts_free_int_and_float_axes() {
    let s = scan(&ind(
        "a = input.int(1)\nb = input.float(2.0)\nc = input.int(3, options = [1, 2, 3])\nd = input.int(4)\ne = input.bool(true)\nplot(close * a * b * c * (e ? 1 : 0))",
    ));
    // a, b counted; c has options; d is dead; e is a bool.
    assert_eq!(k_numeric(&s.inputs), 2);
}
