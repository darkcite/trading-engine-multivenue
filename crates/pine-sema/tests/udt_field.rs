// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! R-P5 — the UDT field rows (`array.sort` / `sort_indices` / `binary_search*` with
//! `sort_field`, v6): a field by name or index folds to its index and type, the selector reaches
//! pine-std as `index | (Ty << 32)` (`pin:02-udt-field-selector`), a field of another type or a
//! bad selector refuses `3007`, a non-UDT array keeps `3024 … unserved=1`
//! (`pin:02-udt-rows-udt-only`).

mod common;

use common::{all_diags, codes, compile_src, default_opts, is_unserved, ACCEPTED};
use pine_ir::{argwin, op, Dialect, ExecMode, OverloadId, PineProgram, Ty};
use pine_spec::ov;

const HEAD: &str = "//@version=6\nindicator(\"t\")\ntype Q\n    int k\n\
                    type P\n    int id\n    float px\n    string name\n    bool on\n    \
                    color c\n    Q q\n\
                    a = array.new<P>()\n\
                    array.push(a, P.new(1, close, \"x\", true, color.red, Q.new(1)))\n";

fn v6(body: &str) -> Result<PineProgram, Vec<pine_ir::Diag>> {
    let src = format!("{HEAD}{body}plot(close)\n");
    compile_src(&src, &default_opts(Dialect::V6, ExecMode::BarClose))
}

/// The selector constant of the first call of `ov`: window entry `at` (`sort_field`), read
/// back from the init block's `I_CONST` of its slot.
fn selector(p: &PineProgram, ov: OverloadId, at: usize) -> i64 {
    let call = p
        .code
        .iter()
        .find(|i| i.op == op::CALL_BUILTIN && i.a == ov.0)
        .unwrap_or_else(|| panic!("no call of {ov:?}"));
    let w = usize::from(call.b);
    let e = p.argwin[w + 1 + at];
    assert!(argwin::is_i64(e), "the selector is an i64 entry");
    let slot = argwin::slot(e);
    let k = p
        .code
        .iter()
        .find(|i| i.op == op::I_CONST && i.dst == slot)
        .expect("the selector is a constant");
    p.consts[usize::from(k.a)] as i64
}

fn sel(i: i64, ty: Ty) -> i64 {
    i | (ty as i64) << 32
}

/// A field by name and by index resolves to its index and type; the omitted `sort_field` is the
/// row's default, field 0.
#[test]
fn udt_field_by_name_or_index_folds_to_selector() {
    let p = v6("array.sort(a, order.ascending, \"px\")\n").expect("compiles");
    assert_eq!(selector(&p, ov::ARRAY_SORT_AT, 2), sel(1, Ty::Float));
    let p = v6("i = array.sort_indices(a, order.descending, 2)\n").expect("compiles");
    // `[__elem, __key, __cap, id, order, sort_field, out]`.
    assert_eq!(
        selector(&p, ov::ARRAY_SORT_INDICES_AT, 5),
        sel(2, Ty::String)
    );
    let p = v6("array.sort(a, order.descending, \"name\")\n").expect("compiles");
    assert_eq!(selector(&p, ov::ARRAY_SORT_AT, 2), sel(2, Ty::String));
    let p = v6("array.sort(a, order.ascending)\n").expect("compiles");
    assert_eq!(selector(&p, ov::ARRAY_SORT_AT, 2), sel(0, Ty::Int));
    for (row, f) in [
        ("binary_search", ov::ARRAY_BINARY_SEARCH_AT),
        (
            "binary_search_leftmost",
            ov::ARRAY_BINARY_SEARCH_LEFTMOST_AT,
        ),
        (
            "binary_search_rightmost",
            ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AT,
        ),
    ] {
        let p = v6(&format!("x = array.{row}(a, 2.5, \"px\")\n")).expect("compiles");
        assert_eq!(selector(&p, f, 2), sel(1, Ty::Float), "{row}");
        let p = v6(&format!("x = array.{row}(a, 3, 0)\n")).expect("compiles");
        assert_eq!(selector(&p, f, 2), sel(0, Ty::Int), "{row}");
    }
}

/// A `bool`, `color` or UDT field refuses `3007` for every row; a `string` field refuses for the
/// searches only; so does an unknown name, an index out of range and a string `val`.
#[test]
fn udt_field_of_other_type_or_bad_selector_refuses_3007() {
    for f in ["\"on\"", "4", "\"q\""] {
        for call in [
            format!("array.sort(a, order.ascending, {f})\n"),
            format!("i = array.sort_indices(a, order.ascending, {f})\n"),
            format!("x = array.binary_search(a, 1.0, {f})\n"),
            format!("x = array.binary_search_leftmost(a, 1.0, {f})\n"),
            format!("x = array.binary_search_rightmost(a, 1.0, {f})\n"),
        ] {
            assert_eq!(codes(&v6(&call)), vec![3007], "{call}");
        }
    }
    for call in [
        "x = array.binary_search(a, 1.0, \"name\")\n",
        "x = array.binary_search_rightmost(a, 1.0, 2)\n",
        "array.sort(a, order.ascending, \"nope\")\n",
        "array.sort(a, order.ascending, 6)\n",
        "array.sort(a, order.ascending, -1)\n",
        "x = array.binary_search(a, \"x\", \"px\")\n",
    ] {
        assert_eq!(codes(&v6(call)), vec![3007], "{call}");
    }
    // Field 0 (`int id`) is the default: a search of an int field compiles.
    assert_eq!(
        codes(&v6("x = array.binary_search(a, 1)\n")),
        vec![ACCEPTED]
    );
}

/// `pin:02-udt-rows-udt-only`: an `*_AT` row over a non-UDT array stays `3024 … unserved=1`.
#[test]
fn udt_rows_over_scalar_arrays_stay_unserved() {
    for body in [
        "b = array.new<bool>()\narray.sort(b)\n",
        "b = array.new<float>()\narray.sort(b, order.ascending, 0)\n",
    ] {
        let ds = all_diags(v6(body));
        assert!(
            !ds.is_empty() && ds.iter().all(is_unserved),
            "{body}: {ds:?}"
        );
    }
}
