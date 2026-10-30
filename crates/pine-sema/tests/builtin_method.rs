// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A built-in method call — `a.push(x)`, `m.put(k, v)`, `mx.set(r, c, x)`
//! (`pin:02-method-namespace`: `<ns>.<m>` with `<ns>` the receiver's `array` / `map` / `matrix`)
//! — lowers exactly as its function form `array.push(a, x)`: the receiver binds the first
//! parameter row and the written arguments follow it (TradingView reference, "Methods":
//! `array<float> a = array.new<float>()`, `a.push(close)`). Infer binds the rows over the
//! receiver-first argument list; lowering walked the written arguments alone, so every argument
//! landed one row off and a required row was left without one (`3255`).

mod common;

use common::{compile_src, default_opts};
use pine_ir::{Dialect, ExecMode, PineProgram};

fn compile(d: Dialect, body: &str) -> PineProgram {
    let v = if d == Dialect::V5 { 5 } else { 6 };
    let src = format!("//@version={v}\nindicator(\"t\")\n{body}");
    compile_src(&src, &default_opts(d, ExecMode::BarClose))
        .unwrap_or_else(|e| panic!("v{v} refused:\n{body}{e:?}"))
}

/// The method form and the function form give the same code, constants, argument windows and
/// layout (spans and the symbol table's text are the written source's).
fn same_as_function_form(d: Dialect, method: &str, function: &str) {
    let m = compile(d, method);
    let f = compile(d, function);
    assert_eq!(m.code, f.code, "{d:?} code:\n{method}");
    assert_eq!(m.consts, f.consts, "{d:?} consts:\n{method}");
    assert_eq!(m.argwin, f.argwin, "{d:?} argument windows:\n{method}");
    assert_eq!(m.layout, f.layout, "{d:?} layout:\n{method}");
}

const ARRAY_M: &str = "a = array.new<float>()\na.push(close)\na.push(open)\nx = a.get(0)\n\
                       a.set(1, x + 1)\nn = a.size()\na.sort(order.descending)\ny = a.pop()\n\
                       plot(x + n + y)\n";
const ARRAY_F: &str = "a = array.new<float>()\narray.push(a, close)\narray.push(a, open)\n\
                       x = array.get(a, 0)\narray.set(a, 1, x + 1)\nn = array.size(a)\n\
                       array.sort(a, order.descending)\ny = array.pop(a)\nplot(x + n + y)\n";

/// `array`: `push`, `get`, `set`, `size`, `sort` (a non-default `order`), `pop`, v5 and v6.
#[test]
fn builtin_method_array_lowers_as_function_form() {
    for d in [Dialect::V5, Dialect::V6] {
        same_as_function_form(d, ARRAY_M, ARRAY_F);
    }
}

/// A named argument after the receiver binds by name, as in the function form.
#[test]
fn builtin_method_named_argument_binds_by_name() {
    let m = "a = array.new<float>()\na.push(close)\na.sort(order = order.descending)\n\
             plot(a.get(index = 0))\n";
    let f = "a = array.new<float>()\narray.push(a, close)\n\
             array.sort(a, order = order.descending)\nplot(array.get(a, index = 0))\n";
    for d in [Dialect::V5, Dialect::V6] {
        same_as_function_form(d, m, f);
    }
}

/// `map`: `put`, `get`, `contains`, `size`.
#[test]
fn builtin_method_map_lowers_as_function_form() {
    let m = "m = map.new<string, float>()\nm.put(\"k\", close)\ng = m.get(\"k\")\n\
             c = m.contains(\"k\")\nplot(c ? g + m.size() : 0.0)\n";
    let f = "m = map.new<string, float>()\nmap.put(m, \"k\", close)\ng = map.get(m, \"k\")\n\
             c = map.contains(m, \"k\")\nplot(c ? g + map.size(m) : 0.0)\n";
    for d in [Dialect::V5, Dialect::V6] {
        same_as_function_form(d, m, f);
    }
}

/// `matrix` (tier 4, outside the default `active_tiers`): the method form meets the function
/// form's own refusal, nothing else.
#[test]
fn builtin_method_matrix_refuses_as_function_form() {
    let m = "mx = matrix.new<float>(2, 2, 0.0)\nmx.set(0, 1, close)\n\
             plot(mx.get(0, 1) + mx.rows())\n";
    let f = "mx = matrix.new<float>(2, 2, 0.0)\nmatrix.set(mx, 0, 1, close)\n\
             plot(matrix.get(mx, 0, 1) + matrix.rows(mx))\n";
    for d in [Dialect::V5, Dialect::V6] {
        let v = if d == Dialect::V5 { 5 } else { 6 };
        let o = default_opts(d, ExecMode::BarClose);
        let refusal = |body: &str| -> Vec<(u16, String)> {
            let src = format!("//@version={v}\nindicator(\"t\")\n{body}");
            match compile_src(&src, &o) {
                Ok(_) => panic!("v{v} compiled:\n{body}"),
                Err(ds) => ds.into_iter().map(|x| (x.code.0, x.msg)).collect(),
            }
        };
        let (rm, rf) = (refusal(m), refusal(f));
        assert_eq!(rm, rf, "v{v}");
        assert!(rm.iter().all(|r| r.0 == 3024), "v{v}: {rm:?}");
    }
}

/// An `array<UDT>` receiver, a field read as an argument and a method call's element read as a
/// field's base (`a.get(0).v`).
#[test]
fn builtin_method_on_udt_array_lowers_as_function_form() {
    let head = "type P\n    float v = 0.0\np = P.new(close)\na = array.new<P>()\n";
    let m = format!("{head}a.push(p)\na.push(P.new(p.v + 1))\nplot(a.get(1).v + a.size())\n");
    let f = format!(
        "{head}array.push(a, p)\narray.push(a, P.new(p.v + 1))\n\
         plot(array.get(a, 1).v + array.size(a))\n"
    );
    for d in [Dialect::V5, Dialect::V6] {
        same_as_function_form(d, &m, &f);
    }
}

/// A user method of a built-in method's name still wins (§4.5 step 7) and binds its receiver
/// (CI-012) — `plot(a.push(1.0))` reads its `float`, the built-in `array.push` has no value — and
/// the built-in methods in and beside it lower as their function forms.
#[test]
fn builtin_method_beside_user_method_of_same_name() {
    let m = "method push(array<float> a, float v) => a.size() + v\na = array.new<float>()\n\
             a.unshift(close)\nplot(a.push(1.0))\n";
    let f = "method push(array<float> a, float v) => array.size(a) + v\na = array.new<float>()\n\
             array.unshift(a, close)\nplot(a.push(1.0))\n";
    for d in [Dialect::V5, Dialect::V6] {
        same_as_function_form(d, m, f);
    }
}
